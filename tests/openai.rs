//! End to end: every inbound protocol against every upstream protocol, streaming and not.

mod support;

use airmate_core::sse::SseParser;
use serde_json::{json, Value};
use support::{env_with_key, yaml_for, Mock, Part, Reply, TestGateway};

const UP_KEY: &str = "sk-up";

#[derive(Clone, Copy, Debug)]
enum P {
    Anthropic,
    Chat,
    Responses,
}

impl P {
    fn name(self) -> &'static str {
        match self {
            P::Anthropic => "anthropic",
            P::Chat => "chat",
            P::Responses => "responses",
        }
    }

    /// A model name in this protocol's own family, so it resolves through the table.
    fn model(self) -> &'static str {
        match self {
            P::Anthropic => "claude-x",
            P::Chat | P::Responses => "gpt-4.1",
        }
    }

    fn path(self) -> &'static str {
        match self {
            P::Anthropic => "/v1/messages",
            P::Chat => "/v1/chat/completions",
            P::Responses => "/v1/responses",
        }
    }
}

const ALL: [P; 3] = [P::Anthropic, P::Chat, P::Responses];

/// A minimal request in `p`'s protocol.
fn request(p: P, stream: bool) -> Value {
    match p {
        P::Anthropic => {
            json!({"model": p.model(), "max_tokens": 50, "stream": stream, "messages": [{"role": "user", "content": "say hello"}]})
        }
        P::Chat => {
            json!({"model": p.model(), "max_tokens": 50, "stream": stream, "stream_options": {"include_usage": true}, "messages": [{"role": "user", "content": "say hello"}]})
        }
        P::Responses => {
            json!({"model": p.model(), "max_output_tokens": 50, "stream": stream, "input": "say hello"})
        }
    }
}

/// The upstream's non-streaming answer: "Hello" plus a tool call, usage 10 in / 3 out.
fn upstream_json(p: P) -> Value {
    match p {
        P::Chat => {
            json!({"id": "c", "model": "upstream", "choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello",
            "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"p\":1}"}}]}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 3}})
        }
        P::Responses => json!({"id": "r", "model": "upstream", "status": "completed", "output": [
            {"type": "message", "content": [{"type": "output_text", "text": "Hello"}]},
            {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"p\":1}"}],
            "usage": {"input_tokens": 10, "output_tokens": 3}}),
        P::Anthropic => {
            json!({"id": "m", "type": "message", "role": "assistant", "model": "upstream", "stop_reason": "tool_use", "content": [
            {"type": "text", "text": "Hello"}, {"type": "tool_use", "id": "call_1", "name": "read", "input": {"p": 1}}],
            "usage": {"input_tokens": 10, "output_tokens": 3}})
        }
    }
}

fn sse(parts: &[(&str, Value)]) -> Vec<Part> {
    parts
        .iter()
        .map(|(ev, d)| {
            Part::Bytes(
                if ev.is_empty() {
                    format!("data: {d}\n\n")
                } else {
                    format!("event: {ev}\ndata: {d}\n\n")
                }
                .into_bytes(),
            )
        })
        .collect()
}

/// The upstream's streaming answer, saying the same thing.
fn upstream_stream(p: P) -> Vec<Part> {
    match p {
        P::Chat => {
            let c = |delta: Value, finish: Value| json!({"id": "c", "model": "upstream", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
            let mut parts = sse(&[
                (
                    "",
                    c(json!({"role": "assistant", "content": "Hel"}), Value::Null),
                ),
                ("", c(json!({"content": "lo"}), Value::Null)),
                (
                    "",
                    c(
                        json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "read", "arguments": "{\"p\":"}}]}),
                        Value::Null,
                    ),
                ),
                (
                    "",
                    c(
                        json!({"tool_calls": [{"index": 0, "function": {"arguments": "1}"}}]}),
                        Value::Null,
                    ),
                ),
                ("", c(json!({}), json!("tool_calls"))),
                (
                    "",
                    json!({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 3}}),
                ),
            ]);
            parts.push(Part::Bytes(b"data: [DONE]\n\n".to_vec()));
            parts
        }
        P::Responses => sse(&[
            (
                "response.created",
                json!({"type": "response.created", "response": {"id": "r", "model": "upstream"}}),
            ),
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "message"}}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "output_index": 0, "delta": "Hel"}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "output_index": 0, "delta": "lo"}),
            ),
            (
                "response.output_item.done",
                json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "message"}}),
            ),
            (
                "response.output_item.added",
                json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "call_id": "call_1", "name": "read"}}),
            ),
            (
                "response.function_call_arguments.delta",
                json!({"type": "response.function_call_arguments.delta", "output_index": 1, "delta": "{\"p\":1}"}),
            ),
            (
                "response.output_item.done",
                json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "function_call"}}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": {"usage": {"input_tokens": 10, "output_tokens": 3}}}),
            ),
        ]),
        P::Anthropic => sse(&[
            (
                "message_start",
                json!({"type": "message_start", "message": {"id": "m", "model": "upstream", "usage": {"input_tokens": 10, "output_tokens": 1}}}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Hel"}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            (
                "content_block_start",
                json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "call_1", "name": "read", "input": {}}}),
            ),
            (
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"p\":1}"}}),
            ),
            (
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 1}),
            ),
            (
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 3}}),
            ),
            ("message_stop", json!({"type": "message_stop"})),
        ]),
    }
}

/// What a client of protocol `p` reads out of a (non-streaming) response.
struct Seen {
    text: String,
    call: (String, Value),
    input: u64,
    output: u64,
    model: String,
}

fn read_json(p: P, v: &Value) -> Seen {
    match p {
        P::Anthropic => {
            let text = v["content"][0]["text"].as_str().unwrap().to_owned();
            let c = &v["content"][1];
            Seen {
                text,
                call: (c["name"].as_str().unwrap().into(), c["input"].clone()),
                input: v["usage"]["input_tokens"].as_u64().unwrap(),
                output: v["usage"]["output_tokens"].as_u64().unwrap(),
                model: v["model"].as_str().unwrap().into(),
            }
        }
        P::Chat => {
            let m = &v["choices"][0]["message"];
            let c = &m["tool_calls"][0]["function"];
            Seen {
                text: m["content"].as_str().unwrap().into(),
                call: (
                    c["name"].as_str().unwrap().into(),
                    serde_json::from_str(c["arguments"].as_str().unwrap()).unwrap(),
                ),
                input: v["usage"]["prompt_tokens"].as_u64().unwrap(),
                output: v["usage"]["completion_tokens"].as_u64().unwrap(),
                model: v["model"].as_str().unwrap().into(),
            }
        }
        P::Responses => {
            let c = &v["output"][1];
            Seen {
                text: v["output"][0]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .into(),
                call: (
                    c["name"].as_str().unwrap().into(),
                    serde_json::from_str(c["arguments"].as_str().unwrap()).unwrap(),
                ),
                input: v["usage"]["input_tokens"].as_u64().unwrap(),
                output: v["usage"]["output_tokens"].as_u64().unwrap(),
                model: v["model"].as_str().unwrap().into(),
            }
        }
    }
}

fn read_stream(p: P, body: &[u8]) -> Seen {
    let mut parser = SseParser::new();
    let mut evs = parser.push(body).unwrap();
    evs.extend(parser.finish().unwrap());
    let js: Vec<Value> = evs
        .iter()
        .filter(|e| e.data != "[DONE]")
        .map(|e| serde_json::from_str(&e.data).unwrap())
        .collect();
    let (mut text, mut args, mut name) = (String::new(), String::new(), String::new());
    let (mut input, mut output, mut model) = (0, 0, String::new());
    for j in &js {
        match p {
            P::Anthropic => {
                if let Some(t) = j.pointer("/delta/text").and_then(Value::as_str) {
                    text.push_str(t);
                }
                if let Some(t) = j.pointer("/delta/partial_json").and_then(Value::as_str) {
                    args.push_str(t);
                }
                if let Some(n) = j.pointer("/content_block/name").and_then(Value::as_str) {
                    name = n.into();
                }
                if j["type"] == "message_start" {
                    model = j["message"]["model"].as_str().unwrap().into();
                    input = j["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
                }
                if j["type"] == "message_delta" {
                    input = j["usage"]["input_tokens"]
                        .as_u64()
                        .filter(|n| *n > 0)
                        .unwrap_or(input);
                    output = j["usage"]["output_tokens"].as_u64().unwrap_or(0);
                }
            }
            P::Chat => {
                let d = &j["choices"][0]["delta"];
                if let Some(t) = d["content"].as_str() {
                    text.push_str(t);
                }
                if let Some(t) = d["tool_calls"][0]["function"]["arguments"].as_str() {
                    args.push_str(t);
                }
                if let Some(n) = d["tool_calls"][0]["function"]["name"].as_str() {
                    name = n.into();
                }
                if let Some(m) = j["model"].as_str() {
                    model = m.into();
                }
                if j["usage"].is_object() {
                    input = j["usage"]["prompt_tokens"].as_u64().unwrap();
                    output = j["usage"]["completion_tokens"].as_u64().unwrap();
                }
            }
            P::Responses => {
                if j["type"] == "response.output_text.delta" {
                    text.push_str(j["delta"].as_str().unwrap());
                }
                if j["type"] == "response.function_call_arguments.delta" {
                    args.push_str(j["delta"].as_str().unwrap());
                }
                if j["type"] == "response.output_item.added" && j["item"]["type"] == "function_call"
                {
                    name = j["item"]["name"].as_str().unwrap().into();
                }
                if j["type"] == "response.created" {
                    model = j["response"]["model"].as_str().unwrap().into();
                }
                if j["type"] == "response.completed" {
                    input = j["response"]["usage"]["input_tokens"].as_u64().unwrap();
                    output = j["response"]["usage"]["output_tokens"].as_u64().unwrap();
                }
            }
        }
    }
    Seen {
        text,
        call: (name, serde_json::from_str(&args).unwrap()),
        input,
        output,
        model,
    }
}

async fn gateway(mock: &Mock, up: P) -> TestGateway {
    let yaml = yaml_for(&mock.url(), up.name(), "generic", "", "");
    TestGateway::start_with_env(&yaml, &env_with_key("UP_KEY", UP_KEY)).await
}

#[tokio::test]
async fn every_inbound_reaches_every_upstream_without_streaming() {
    for inbound in ALL {
        for up in ALL {
            let reply = upstream_json(up);
            let mock = Mock::start(move |_| Reply::json(200, reply.clone())).await;
            let gw = gateway(&mock, up).await;
            let r = gw
                .post_with(
                    inbound.path(),
                    &request(inbound, false),
                    &[("authorization", "Bearer test-gateway-token")],
                )
                .await;
            assert_eq!(r.status, 200, "{inbound:?} -> {up:?}: {}", r.text());
            let seen = read_json(inbound, &r.json());
            assert_eq!(seen.text, "Hello", "{inbound:?} -> {up:?}");
            assert_eq!(
                seen.call,
                ("read".to_owned(), json!({"p": 1})),
                "{inbound:?} -> {up:?}"
            );
            assert_eq!((seen.input, seen.output), (10, 3), "{inbound:?} -> {up:?}");
            assert_eq!(
                seen.model,
                inbound.model(),
                "the client's model name is echoed ({inbound:?} -> {up:?})"
            );
            let sent = &mock.recorded()[0];
            assert!(
                sent.path.starts_with(match up {
                    P::Anthropic => "/v1/messages",
                    P::Chat => "/v1/chat/completions",
                    P::Responses => "/v1/responses",
                }),
                "{inbound:?} -> {up:?}: {}",
                sent.path
            );
            assert_eq!(sent.json()["model"], "upstream-model");
            match up {
                P::Anthropic => assert_eq!(sent.header("x-api-key"), Some(UP_KEY)),
                _ => assert_eq!(
                    sent.header("authorization"),
                    Some(&*format!("Bearer {UP_KEY}"))
                ),
            }
            assert!(
                !format!("{:?}", sent.headers).contains("test-gateway-token"),
                "the gateway token never reaches the upstream"
            );
        }
    }
}

#[tokio::test]
async fn every_inbound_reaches_every_upstream_with_streaming() {
    for inbound in ALL {
        for up in ALL {
            let parts = upstream_stream(up);
            let mock = Mock::start(move |_| Reply::sse(parts.clone())).await;
            let gw = gateway(&mock, up).await;
            let r = gw
                .post_with(
                    inbound.path(),
                    &request(inbound, true),
                    &[("authorization", "Bearer test-gateway-token")],
                )
                .await;
            assert_eq!(r.status, 200, "{inbound:?} -> {up:?}: {}", r.text());
            assert_eq!(r.header("content-type"), Some("text/event-stream"));
            let seen = read_stream(inbound, &r.body);
            assert_eq!(seen.text, "Hello", "{inbound:?} -> {up:?}\n{}", r.text());
            assert_eq!(
                seen.call,
                ("read".to_owned(), json!({"p": 1})),
                "{inbound:?} -> {up:?}"
            );
            assert_eq!((seen.input, seen.output), (10, 3), "{inbound:?} -> {up:?}");
            assert_eq!(seen.model, inbound.model(), "{inbound:?} -> {up:?}");
        }
    }
}

#[tokio::test]
async fn errors_come_back_in_the_inbound_protocols_envelope() {
    let mock = Mock::start(|_| Reply::json(429, json!({"error": {"message": "slow"}}))).await;
    let gw = gateway(&mock, P::Chat).await;
    let r = gw.post("/v1/messages", &request(P::Anthropic, false)).await;
    assert_eq!(
        (
            r.status,
            r.json()["type"].as_str(),
            r.json()["error"]["type"].as_str()
        ),
        (429, Some("error"), Some("rate_limit_error"))
    );
    for p in [P::Chat, P::Responses] {
        let r = gw.post(p.path(), &request(p, false)).await;
        assert_eq!(r.status, 429, "{p:?}");
        let j = r.json();
        assert_eq!(
            (
                j["error"]["type"].as_str(),
                j["error"]["code"].as_str(),
                j["error"]["message"].as_str()
            ),
            (
                Some("rate_limit_error"),
                Some("rate_limit_exceeded"),
                Some("slow")
            ),
            "{p:?}"
        );
        assert!(
            j.get("request_id").is_none(),
            "OpenAI bodies carry no request_id"
        );
        assert!(r.header("request-id").is_some());
    }
    let unauth = gw
        .request(
            "POST",
            "/v1/chat/completions",
            Some("{}".into()),
            &[("x-api-key", "wrong")],
        )
        .await;
    assert_eq!(unauth.status, 401);
    assert_eq!(unauth.json()["error"]["code"], "invalid_api_key");
}

#[tokio::test]
async fn same_protocol_pairs_are_passed_through_untouched() {
    for p in [P::Chat, P::Responses] {
        let reply = upstream_json(p);
        let mock = Mock::start(move |_| Reply::json(200, reply.clone())).await;
        let gw = gateway(&mock, p).await;
        let mut req = request(p, false);
        req["something_only_this_upstream_knows"] = json!({"a": 1});
        let r = gw.post(p.path(), &req).await;
        assert_eq!(r.status, 200, "{p:?}: {}", r.text());
        assert_eq!(r.json()["model"], p.model());
        let sent = mock.recorded()[0].json();
        assert_eq!(
            sent["something_only_this_upstream_knows"],
            json!({"a": 1}),
            "{p:?}: the body is forwarded as sent"
        );
        assert_eq!(sent["model"], "upstream-model");
        assert!(r.header("x-airmate-ignored").is_none());
    }
}

#[tokio::test]
async fn a_chat_passthrough_stream_hides_a_usage_chunk_the_client_did_not_request() {
    let parts = upstream_stream(P::Chat);
    let mock = Mock::start(move |_| Reply::sse(parts.clone())).await;
    let gw = gateway(&mock, P::Chat).await;
    let mut req = request(P::Chat, true);
    req.as_object_mut().unwrap().remove("stream_options");
    let r = gw.post(P::Chat.path(), &req).await;
    assert!(!r.text().contains("\"usage\""), "{}", r.text());
    assert!(r.text().contains("[DONE]"));
    assert_eq!(
        mock.recorded()[0].json()["stream_options"]["include_usage"],
        true,
        "the upstream is still asked, so the call is accounted for"
    );
    let r = gw.post(P::Chat.path(), &request(P::Chat, true)).await;
    assert!(
        r.text().contains("\"usage\""),
        "a client that asked gets the usage chunk"
    );
    assert!(
        r.text().contains("\"model\":\"gpt-4.1\"") && !r.text().contains("\"model\":\"upstream\"")
    );
}

#[tokio::test]
async fn models_are_listed_in_the_shape_the_client_expects() {
    let mock = Mock::start(|_| Reply::json(200, json!({}))).await;
    let yaml = format!("upstreams:\n  - name: up\n    url: {}\nmodels:\n  - match: gpt-4.1\n    targets: [ {{ upstream: up, model: m }} ]\n", mock.url());
    let gw = TestGateway::start(&yaml).await;
    let anthropic = gw
        .request(
            "GET",
            "/v1/models",
            None,
            &[
                ("anthropic-version", "2023-06-01"),
                ("x-api-key", "test-gateway-token"),
            ],
        )
        .await
        .json();
    assert_eq!(
        (
            anthropic["data"][0]["type"].as_str(),
            anthropic["has_more"].as_bool()
        ),
        (Some("model"), Some(false))
    );
    // An OpenAI client sends a bearer token and no anthropic-version header.
    let r = gw
        .request(
            "GET",
            "/v1/models",
            None,
            &[("authorization", "Bearer test-gateway-token")],
        )
        .await;
    assert_eq!(r.status, 200);
    let j = r.json();
    assert_eq!(
        (
            j["object"].as_str(),
            j["data"][0]["object"].as_str(),
            j["data"][0]["id"].as_str()
        ),
        (Some("list"), Some("model"), Some("gpt-4.1"))
    );
    let one = gw
        .request(
            "GET",
            "/v1/models/gpt-4.1",
            None,
            &[("authorization", "Bearer test-gateway-token")],
        )
        .await
        .json();
    assert_eq!(one["object"], "model");
}

#[tokio::test]
async fn an_anthropic_client_can_replay_reasoning_through_a_responses_upstream_that_encrypts_it() {
    // Turn one: the upstream returns a reasoning item with encrypted content.
    let first = json!({"id": "r", "model": "u", "status": "completed", "output": [
        {"type": "reasoning", "id": "rs", "summary": [{"type": "summary_text", "text": "thinking"}], "encrypted_content": "UPSTREAM-BLOB"},
        {"type": "message", "content": [{"type": "output_text", "text": "answer"}]}], "usage": {"input_tokens": 1, "output_tokens": 1}});
    let mock = Mock::start(move |_| Reply::json(200, first.clone())).await;
    let gw = gateway(&mock, P::Responses).await;
    let mut req = json!({"model": "claude-x", "max_tokens": 4096, "thinking": {"type": "enabled", "budget_tokens": 2048},
        "messages": [{"role": "user", "content": "q"}]});
    let r = gw.post("/v1/messages", &req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let content = r.json()["content"].clone();
    assert_eq!(content[0]["type"], "thinking");
    let sig = content[0]["signature"].as_str().unwrap();
    assert!(
        sig.starts_with("am1."),
        "the client holds the gateway's signature: {sig}"
    );
    assert!(!sig.contains("UPSTREAM-BLOB"));

    // Turn two: the client echoes its thinking block back, and the upstream gets its own blob again.
    req["messages"] = json!([{"role": "user", "content": "q"}, {"role": "assistant", "content": content}, {"role": "user", "content": "next"}]);
    let r = gw.post("/v1/messages", &req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let sent = mock.recorded()[1].json();
    let items = sent["input"].as_array().unwrap();
    let reasoning = items
        .iter()
        .find(|i| i["type"] == "reasoning")
        .expect("the reasoning item was replayed");
    assert_eq!(reasoning["encrypted_content"], "UPSTREAM-BLOB");
    assert_eq!(sent["store"], false);
}

#[tokio::test]
async fn a_responses_client_can_replay_reasoning_through_an_anthropic_upstream() {
    let first = json!({"id": "m", "type": "message", "role": "assistant", "model": "u", "stop_reason": "end_turn", "content": [
        {"type": "thinking", "thinking": "thinking", "signature": "ANTHROPIC-SIG"}, {"type": "text", "text": "answer"}],
        "usage": {"input_tokens": 1, "output_tokens": 1}});
    let mock = Mock::start(move |_| Reply::json(200, first.clone())).await;
    let gw = gateway(&mock, P::Anthropic).await;
    let mut req = json!({"model": "gpt-4.1", "input": "q", "reasoning": {"effort": "low", "summary": "auto"}, "include": ["reasoning.encrypted_content"]});
    let r = gw.post("/v1/responses", &req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let out = r.json()["output"].clone();
    let blob = out[0]["encrypted_content"].as_str().unwrap();
    assert!(blob.starts_with("am1.") && !blob.contains("ANTHROPIC-SIG"));

    req["input"] = json!([{"role": "user", "content": "q"}, out[0], out[1], {"role": "user", "content": "next"}]);
    let r = gw.post("/v1/responses", &req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let sent = mock.recorded()[1].json();
    let thinking = sent["messages"][1]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("the thinking block was replayed");
    assert_eq!(thinking["signature"], "ANTHROPIC-SIG");
    assert_eq!(thinking["thinking"], "thinking");
}

#[tokio::test]
async fn an_unsigned_reasoning_item_is_dropped_and_reported() {
    let mock = Mock::start(|_| Reply::json(200, upstream_json(P::Responses))).await;
    let gw = gateway(&mock, P::Responses).await;
    let req = json!({"model": "gpt-4.1", "input": [{"role": "user", "content": "q"}, {"type": "reasoning", "summary": [], "encrypted_content": "gAAAA-some-other-gateways-blob"}, {"role": "user", "content": "next"}]});
    // Responses -> Responses is a passthrough, so force a translation with a chat upstream.
    let _ = (req.clone(), &gw);
    let chat = Mock::start(|_| Reply::json(200, upstream_json(P::Chat))).await;
    let gw = gateway(&chat, P::Chat).await;
    let r = gw.post("/v1/responses", &req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r
        .header("x-airmate-ignored")
        .unwrap()
        .contains("thinking_unverified"));
    assert!(!chat.recorded()[0]
        .json()
        .to_string()
        .contains("some-other-gateways-blob"));
}

#[tokio::test]
async fn openai_clients_see_policy_rejections_by_name() {
    let mock = Mock::start(|_| Reply::json(200, upstream_json(P::Anthropic))).await;
    let gw = gateway(&mock, P::Anthropic).await;
    let mut chat = request(P::Chat, false);
    chat["logprobs"] = json!(true);
    let r = gw.post("/v1/chat/completions", &chat).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("logprobs"));
    let mut resp = request(P::Responses, false);
    resp["previous_response_id"] = json!("resp_1");
    let r = gw.post("/v1/responses", &resp).await;
    assert_eq!(r.status, 400);
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("previous_response_id"));
    assert_eq!(mock.count(), 0);
}
