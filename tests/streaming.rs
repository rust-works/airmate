//! End to end: streaming responses, keep-alive, timeouts, truncation, cancellation, backpressure.

mod support;

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use airmate_core::sse::{SseEvent, SseParser};
use serde_json::{json, Value};
use support::{env_with_key, yaml_for, Mock, Part, Reply, TestGateway};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn chunk(delta: Value, finish: Option<&str>) -> Part {
    Part::Bytes(
        format!(
            "data: {}\n\n",
            json!({"id": "c1", "model": "upstream-model", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
        )
        .into_bytes(),
    )
}

fn done() -> Part {
    Part::Bytes(b"data: [DONE]\n\n".to_vec())
}

fn text_stream() -> Vec<Part> {
    vec![
        chunk(json!({"role": "assistant", "content": "Hel"}), None),
        chunk(json!({"content": "lo"}), None),
        chunk(json!({}), Some("stop")),
        Part::Bytes(
            format!(
                "data: {}\n\n",
                json!({"choices": [], "usage": {"prompt_tokens": 7, "completion_tokens": 2}})
            )
            .into_bytes(),
        ),
        done(),
    ]
}

fn request(stream: bool) -> Value {
    json!({"model": "claude-x", "max_tokens": 64, "stream": stream, "messages": [{"role": "user", "content": "hi"}]})
}

fn events(body: &[u8]) -> Vec<SseEvent> {
    let mut p = SseParser::new();
    let mut evs = p.push(body).unwrap();
    evs.extend(p.finish().unwrap());
    evs
}

fn names(evs: &[SseEvent]) -> Vec<String> {
    evs.iter()
        .map(|e| e.event.clone().unwrap_or_default())
        .collect()
}

async fn gateway(
    mock: &Mock,
    protocol: &str,
    extra_upstream: &str,
    extra_top: &str,
) -> TestGateway {
    let yaml = yaml_for(&mock.url(), protocol, "generic", extra_upstream, extra_top);
    TestGateway::start_with_env(&yaml, &env_with_key("UP_KEY", "k")).await
}

#[tokio::test]
async fn a_chat_stream_arrives_as_anthropic_events() {
    let mock = Mock::start(|_| Reply::sse(text_stream())).await;
    let gw = gateway(&mock, "chat", "", "").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("content-type"), Some("text/event-stream"));
    assert_eq!(r.header("cache-control"), Some("no-cache"));
    assert_eq!(r.header("x-airmate-upstream"), Some("up"));
    let evs = events(&r.body);
    assert_eq!(
        names(&evs),
        [
            "message_start",
            "ping",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop"
        ]
    );
    let start: Value = serde_json::from_str(&evs[0].data).unwrap();
    assert_eq!(start["message"]["model"], "claude-x");
    let delta: Value = serde_json::from_str(&evs[6].data).unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(delta["usage"]["input_tokens"], 7);
    assert_eq!(delta["usage"]["output_tokens"], 2);
    let sent = mock.recorded()[0].json();
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"], json!({"include_usage": true}));
}

#[tokio::test]
async fn delivery_in_tiny_slow_chunks_changes_nothing() {
    let whole = Mock::start(|_| Reply::sse(text_stream())).await;
    let wire: Vec<u8> = text_stream()
        .into_iter()
        .flat_map(|p| if let Part::Bytes(b) = p { b } else { vec![] })
        .collect();
    let pieces = Mock::start(move |_| {
        let mut parts = Vec::new();
        for c in wire.chunks(7) {
            parts.push(Part::Bytes(c.to_vec()));
            parts.push(Part::Delay(Duration::from_millis(2)));
        }
        Reply::sse(parts)
    })
    .await;
    let a = gateway(&whole, "chat", "", "")
        .await
        .post("/v1/messages", &request(true))
        .await;
    let b = gateway(&pieces, "chat", "", "")
        .await
        .post("/v1/messages", &request(true))
        .await;
    let norm = |r: &support::Resp| -> Vec<(String, Value)> {
        events(&r.body)
            .into_iter()
            .map(|e| (e.event.unwrap(), serde_json::from_str(&e.data).unwrap()))
            .collect()
    };
    let (na, nb) = (norm(&a), norm(&b));
    assert_eq!(na.len(), nb.len());
    for ((ea, ja), (eb, jb)) in na.iter().zip(&nb) {
        assert_eq!(ea, eb);
        if ea == "message_start" {
            continue; // the message ids differ
        }
        if ea != "content_block_delta" {
            assert_eq!(ja, jb, "{ea}");
        }
    }
    let text = |n: &[(String, Value)]| -> String {
        n.iter()
            .filter_map(|(_, j)| j.pointer("/delta/text").and_then(Value::as_str))
            .collect()
    };
    assert_eq!(text(&na), "Hello");
    assert_eq!(text(&nb), "Hello");
}

#[tokio::test]
async fn text_is_forwarded_live_not_buffered_to_the_end() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![
            chunk(json!({"role": "assistant", "content": "first"}), None),
            Part::Delay(Duration::from_millis(1500)),
            chunk(json!({"content": " second"}), None),
            chunk(json!({}), Some("stop")),
            done(),
        ])
    })
    .await;
    let gw = gateway(&mock, "chat", "", "").await;
    let mut s = tokio::net::TcpStream::connect(gw.addr).await.unwrap();
    let body = request(true).to_string();
    s.write_all(
        format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nx-api-key: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nConnection: close\r\n\r\n{}", gw.addr.port(), gw.token, body.len(), body)
            .as_bytes(),
    )
    .await
    .unwrap();
    let started = Instant::now();
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("first") {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0, "closed before the first text arrived");
        seen.extend_from_slice(&buf[..n]);
    }
    assert!(
        started.elapsed() < Duration::from_millis(1200),
        "the first text arrived after {:?}, before the upstream finished",
        started.elapsed()
    );
}

#[tokio::test]
async fn silence_from_the_upstream_produces_keepalive_pings() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![
            chunk(json!({"role": "assistant", "content": "a"}), None),
            Part::Delay(Duration::from_millis(2600)),
            chunk(json!({}), Some("stop")),
            done(),
        ])
    })
    .await;
    let gw = gateway(&mock, "chat", "", "server:\n  keepalive_secs: 1\n").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    let evs = events(&r.body);
    let pings = names(&evs).iter().filter(|n| *n == "ping").count();
    assert!(
        pings >= 3,
        "one from message_start plus at least two keep-alives, got {pings}: {:?}",
        names(&evs)
    );
    assert_eq!(names(&evs).last().map(String::as_str), Some("message_stop"));
}

#[tokio::test]
async fn a_silent_upstream_ends_the_stream_with_an_error_after_the_idle_timeout() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![
            chunk(json!({"role": "assistant", "content": "a"}), None),
            Part::Hang,
        ])
    })
    .await;
    let gw = gateway(
        &mock,
        "chat",
        "    timeouts: { connect_secs: 2, first_byte_secs: 5, idle_secs: 1 }\n",
        "",
    )
    .await;
    let started = Instant::now();
    let r = gw.post("/v1/messages", &request(true)).await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let evs = events(&r.body);
    let last = evs.last().unwrap();
    assert_eq!(last.event.as_deref(), Some("error"));
    assert!(last.data.contains("silent for 1 seconds"), "{}", last.data);
    assert!(!names(&evs).contains(&"message_stop".to_string()));
}

#[tokio::test]
async fn a_truncated_upstream_stream_ends_in_an_error_never_message_stop() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![chunk(
            json!({"role": "assistant", "content": "half an ans"}),
            None,
        )])
    })
    .await;
    let gw = gateway(&mock, "chat", "", "").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 200);
    let evs = events(&r.body);
    assert_eq!(evs.last().unwrap().event.as_deref(), Some("error"));
    assert!(!names(&evs).contains(&"message_stop".to_string()));
}

#[tokio::test]
async fn a_connection_reset_mid_stream_ends_with_an_error_event() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![
            chunk(json!({"role": "assistant", "content": "x"}), None),
            Part::Delay(Duration::from_millis(200)),
            Part::Reset,
        ])
    })
    .await;
    let gw = gateway(&mock, "chat", "", "").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    let evs = events(&r.body);
    assert_eq!(
        evs.last().unwrap().event.as_deref(),
        Some("error"),
        "{:?}",
        names(&evs)
    );
}

#[tokio::test]
async fn a_reset_before_any_data_arrives_is_a_clean_502() {
    let mock = Mock::start(|_| Reply::sse(vec![Part::Reset])).await;
    let gw = gateway(&mock, "chat", "", "").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 502, "{}", r.text());
    assert_eq!(r.json()["error"]["type"], "api_error");
}

#[tokio::test]
async fn failures_before_the_first_byte_are_http_errors_not_broken_streams() {
    let mock = Mock::start(|_| Reply::json(500, json!({"error": {"message": "boom"}}))).await;
    let gw = gateway(&mock, "chat", "", "").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 500);
    assert_eq!(r.header("content-type"), Some("application/json"));

    let silent = Mock::start(|_| Reply::sse(vec![Part::Hang])).await;
    let gw = gateway(
        &silent,
        "chat",
        "    timeouts: { connect_secs: 2, first_byte_secs: 1, idle_secs: 5 }\n",
        "",
    )
    .await;
    let r = gw.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 504, "{}", r.text());

    let empty = Mock::start(|_| Reply::sse(vec![])).await;
    let gw = gateway(&empty, "chat", "", "").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 502, "{}", r.text());
}

#[tokio::test]
async fn a_client_disconnect_cancels_the_upstream_call() {
    let mock = Mock::start(|_| {
        let mut parts = vec![chunk(
            json!({"role": "assistant", "content": "start"}),
            None,
        )];
        for _ in 0..200 {
            parts.push(Part::Delay(Duration::from_millis(50)));
            parts.push(chunk(json!({"content": " more"}), None));
        }
        Reply::sse(parts)
    })
    .await;
    let gw = gateway(&mock, "chat", "", "").await;
    let mut s = tokio::net::TcpStream::connect(gw.addr).await.unwrap();
    let body = request(true).to_string();
    s.write_all(
        format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nx-api-key: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}", gw.addr.port(), gw.token, body.len(), body)
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 1024];
    while !String::from_utf8_lossy(&seen).contains("start") {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0);
        seen.extend_from_slice(&buf[..n]);
    }
    drop(s);
    let deadline = Instant::now() + Duration::from_secs(5);
    while mock.dropped.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        mock.dropped.load(Ordering::SeqCst) >= 1,
        "the upstream request was cancelled promptly"
    );
    // The concurrency slot was released too.
    assert_eq!(
        gw.shared.permits.available_permits(),
        gw.config.server.max_concurrent as usize
    );
}

#[tokio::test]
async fn a_slow_client_pauses_the_upstream_read() {
    const TOTAL: usize = 8000;
    let payload = "x".repeat(16 * 1024);
    let mock = Mock::start(move |_| {
        let mut parts = vec![chunk(json!({"role": "assistant", "content": "go"}), None)];
        for _ in 0..TOTAL {
            parts.push(chunk(json!({"content": payload}), None));
        }
        parts.push(chunk(json!({}), Some("stop")));
        parts.push(done());
        Reply::sse(parts)
    })
    .await;
    let gw = gateway(&mock, "chat", "", "").await;
    let mut s = tokio::net::TcpStream::connect(gw.addr).await.unwrap();
    let body = request(true).to_string();
    s.write_all(
        format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nx-api-key: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}", gw.addr.port(), gw.token, body.len(), body)
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 2048];
    let _ = s.read(&mut buf).await.unwrap();
    // The client now stops reading. The gateway must not keep pulling the whole stream through.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let sent = mock.sent.load(Ordering::SeqCst);
    assert!(
        sent < TOTAL * 3 / 4,
        "the upstream was read ahead without bound: {sent} of {TOTAL} chunks"
    );
    drop(s);
}

#[tokio::test]
async fn an_anthropic_passthrough_stream_is_forwarded_with_the_model_rewritten() {
    let wire = [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"upstream-model\",\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ];
    let parts: Vec<Part> = wire
        .iter()
        .map(|w| Part::Bytes(w.as_bytes().to_vec()))
        .collect();
    let mock = Mock::start(move |_| Reply::sse(parts.clone())).await;
    let gw = gateway(&mock, "anthropic", "", "").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 200);
    let text = r.text();
    assert!(
        text.contains("\"model\":\"claude-x\"") && !text.contains("upstream-model"),
        "{text}"
    );
    assert!(text.contains("event: message_stop"));
    assert_eq!(mock.recorded()[0].path, "/v1/messages");
}

#[tokio::test]
async fn a_cut_off_passthrough_stream_ends_with_an_error_event() {
    let parts = vec![Part::Bytes(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"u\"}}\n\n".to_vec())];
    let mock = Mock::start(move |_| Reply::sse(parts.clone())).await;
    let gw = gateway(&mock, "anthropic", "", "").await;
    let r = gw.post("/v1/messages", &request(true)).await;
    let evs = events(&r.body);
    assert_eq!(evs.last().unwrap().event.as_deref(), Some("error"));
    assert!(!r.text().contains("message_stop"));
}

#[tokio::test]
async fn a_responses_stream_with_reasoning_and_a_tool_call() {
    let ev = |ty: &str, mut d: Value| -> Part {
        d["type"] = json!(ty);
        Part::Bytes(format!("event: {ty}\ndata: {d}\n\n").into_bytes())
    };
    let parts = vec![
        ev(
            "response.created",
            json!({"response": {"id": "r", "model": "m"}}),
        ),
        ev(
            "response.output_item.added",
            json!({"output_index": 0, "item": {"type": "reasoning"}}),
        ),
        ev(
            "response.reasoning_summary_text.delta",
            json!({"output_index": 0, "summary_index": 0, "delta": "thinking"}),
        ),
        ev(
            "response.output_item.done",
            json!({"output_index": 0, "item": {"type": "reasoning", "encrypted_content": "ENC"}}),
        ),
        ev(
            "response.output_item.added",
            json!({"output_index": 1, "item": {"type": "function_call", "call_id": "c1", "name": "read"}}),
        ),
        ev(
            "response.function_call_arguments.delta",
            json!({"output_index": 1, "delta": "{\"p\":1}"}),
        ),
        ev(
            "response.output_item.done",
            json!({"output_index": 1, "item": {"type": "function_call"}}),
        ),
        ev(
            "response.completed",
            json!({"response": {"usage": {"input_tokens": 3, "output_tokens": 4}}}),
        ),
    ];
    let mock = Mock::start(move |_| Reply::sse(parts.clone())).await;
    let gw = gateway(&mock, "responses", "", "").await;
    let mut req = request(true);
    req["thinking"] = json!({"type": "enabled", "budget_tokens": 2048});
    let r = gw.post("/v1/messages", &req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let text = r.text();
    assert!(
        text.contains("\"type\":\"thinking\"") && text.contains("signature_delta"),
        "{text}"
    );
    assert!(text.contains("\"type\":\"tool_use\"") && text.contains("input_json_delta"));
    assert!(text.contains("\"stop_reason\":\"tool_use\""));
    assert_eq!(mock.recorded()[0].json()["reasoning"]["effort"], "medium");
}

#[tokio::test]
async fn a_streamed_request_is_logged_when_the_stream_ends_with_its_usage() {
    let mock = Mock::start(|_| Reply::sse(text_stream())).await;
    let cap = std::sync::Arc::new(support::Capture::default());
    let yaml = yaml_for(&mock.url(), "chat", "generic", "", "");
    let gw = TestGateway::start_with_sink(&yaml, &env_with_key("UP_KEY", "k"), cap.clone()).await;
    let r = gw.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 200);
    let recs = cap.wait_for(2).await;
    let rec = recs
        .iter()
        .find(|r| r.kind == airmate::obs::RecordKind::Request)
        .unwrap();
    assert!(rec.stream && rec.error_class.is_none());
    assert_eq!((rec.input_tokens, rec.output_tokens), (7, 2));
}

#[tokio::test]
async fn a_cancelled_stream_is_logged_as_client_closed() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![
            chunk(json!({"role": "assistant", "content": "x"}), None),
            Part::Hang,
        ])
    })
    .await;
    let cap = std::sync::Arc::new(support::Capture::default());
    let yaml = yaml_for(&mock.url(), "chat", "generic", "", "");
    let gw = TestGateway::start_with_sink(&yaml, &env_with_key("UP_KEY", "k"), cap.clone()).await;
    let mut s = tokio::net::TcpStream::connect(gw.addr).await.unwrap();
    let body = request(true).to_string();
    s.write_all(
        format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nx-api-key: {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}", gw.addr.port(), gw.token, body.len(), body)
            .as_bytes(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 1024];
    let _ = s.read(&mut buf).await.unwrap();
    drop(s);
    let recs = cap.wait_for(2).await;
    let rec = recs
        .iter()
        .find(|r| r.kind == airmate::obs::RecordKind::Request)
        .expect("the request is logged");
    assert_eq!(rec.error_class.as_deref(), Some("client_closed_request"));
}
