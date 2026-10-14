//! Graceful shutdown: stop accepting, let requests finish, end what is left with an error event.

mod support;

use std::time::{Duration, Instant};

use serde_json::json;
use support::{Mock, Part, Reply, TestGateway};

fn chunk(text: &str) -> Part {
    Part::Bytes(
        format!("data: {{\"id\":\"c\",\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{text}\"}},\"finish_reason\":null}}]}}\n\n")
            .into_bytes(),
    )
}

fn done() -> Vec<Part> {
    vec![
        Part::Bytes(b"data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n".to_vec()),
        Part::Bytes(b"data: [DONE]\n\n".to_vec()),
    ]
}

fn yaml(mock: &Mock, extra: &str) -> String {
    format!(
        "server:\n  affinity: false\n{extra}upstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\n",
        mock.url()
    )
}

fn stream_request() -> serde_json::Value {
    json!({"model": "claude-x", "max_tokens": 8, "stream": true, "messages": [{"role": "user", "content": "hi"}]})
}

#[tokio::test]
async fn a_stream_in_flight_finishes_before_the_gateway_stops() {
    let mock = Mock::start(|_| {
        let mut parts = vec![
            chunk("one "),
            Part::Delay(Duration::from_millis(300)),
            chunk("two "),
        ];
        parts.push(Part::Delay(Duration::from_millis(300)));
        parts.extend(done());
        Reply::sse(parts)
    })
    .await;
    let g = std::sync::Arc::new(TestGateway::start(&yaml(&mock, "")).await);
    let client = {
        let g = g.clone();
        tokio::spawn(async move { g.post("/v1/messages", &stream_request()).await })
    };
    // Shut down once the response has started.
    for _ in 0..100 {
        if mock.count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    g.shutdown_and_wait().await;
    let r = client.await.unwrap();
    assert_eq!(r.status, 200);
    let text = r.text();
    assert!(
        text.contains("one ") && text.contains("two ") && text.contains("message_stop"),
        "the stream ran to its end:\n{text}"
    );
    assert!(!text.contains("event: error"), "{text}");
    assert!(
        started.elapsed() >= Duration::from_millis(400),
        "shutdown waited for the stream"
    );
}

#[tokio::test]
async fn no_new_connections_are_accepted_once_shutdown_begins() {
    let mock = Mock::start(|_| {
        Reply::sse({
            let mut p = vec![chunk("x")];
            p.extend(done());
            p
        })
    })
    .await;
    let g = TestGateway::start(&yaml(&mock, "")).await;
    let addr = g.addr;
    g.shutdown_and_wait().await;
    assert!(
        tokio::net::TcpStream::connect(addr).await.is_err(),
        "the listening socket is closed"
    );
}

#[tokio::test]
async fn a_stream_still_open_at_the_deadline_ends_with_an_error_event() {
    let mock = Mock::start(|_| Reply::sse(vec![chunk("partial "), Part::Hang])).await;
    let g =
        std::sync::Arc::new(TestGateway::start(&yaml(&mock, "  shutdown_timeout_secs: 1\n")).await);
    let client = {
        let g = g.clone();
        tokio::spawn(async move { g.post("/v1/messages", &stream_request()).await })
    };
    for _ in 0..100 {
        if mock.count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    let started = Instant::now();
    g.shutdown_and_wait().await;
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "gave up after the deadline, not never: {:?}",
        started.elapsed()
    );
    let r = tokio::time::timeout(Duration::from_secs(5), client)
        .await
        .expect("the client is released")
        .unwrap();
    let text = r.text();
    assert!(text.contains("partial "), "{text}");
    assert!(
        text.contains("event: error") && text.contains("shutting down"),
        "the client is told why:\n{text}"
    );
    assert!(
        !text.contains("message_stop"),
        "a cut stream never claims to have ended well:\n{text}"
    );
}
