//! The body log: off by default, capped, separate from the request log.

mod support;

use std::sync::{Arc, Mutex};

use airmate::bodylog::{BodyRecord, BodySink};
use serde_json::{json, Value};
use support::{Mock, Part, Reply, TestGateway};

struct Bodies(Mutex<Vec<BodyRecord>>, usize);

impl BodySink for Bodies {
    fn enabled(&self) -> bool {
        true
    }
    fn max_bytes(&self) -> usize {
        self.1
    }
    fn record(&self, rec: &BodyRecord) {
        self.0.lock().unwrap().push(rec.clone());
    }
}

fn chat_ok() -> Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "the answer"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}})
}

fn yaml(mock: &Mock) -> String {
    format!(
        "upstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\n",
        mock.url()
    )
}

fn request(stream: bool) -> Value {
    json!({"model": "claude-x", "max_tokens": 8, "stream": stream, "messages": [{"role": "user", "content": "the question"}]})
}

#[tokio::test]
async fn request_and_response_bodies_are_logged_when_it_is_on() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let bodies = Arc::new(Bodies(Mutex::default(), 64 * 1024));
    let g = TestGateway::start_with_bodies(&yaml(&mock), bodies.clone()).await;
    let r = g.post("/v1/messages", &request(false)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let recs = bodies.0.lock().unwrap().clone();
    assert_eq!(
        recs.iter()
            .map(|r| r.direction.as_str())
            .collect::<Vec<_>>(),
        ["request", "response"]
    );
    assert!(recs[0].body.contains("the question") && !recs[0].truncated);
    assert!(recs[1].body.contains("the answer") && recs[1].status == 200);
    assert_eq!(recs[0].request_id, recs[1].request_id);
    assert_eq!(recs[0].client, "default");
    assert!(
        recs.iter().all(|r| !r.body.contains("test-gateway-token")),
        "the token is a header, never a body"
    );
}

#[tokio::test]
async fn bodies_are_cut_at_the_cap_and_say_so() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let bodies = Arc::new(Bodies(Mutex::default(), 20));
    let g = TestGateway::start_with_bodies(&yaml(&mock), bodies.clone()).await;
    assert_eq!(g.post("/v1/messages", &request(false)).await.status, 200);
    let recs = bodies.0.lock().unwrap().clone();
    assert!(
        recs.iter()
            .all(|r| r.truncated && r.body.len() <= 20 && r.bytes > 20),
        "{recs:?}"
    );
}

#[tokio::test]
async fn a_streamed_response_is_captured_as_sent_to_the_client() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![
            Part::Bytes(b"data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"streamed words\"},\"finish_reason\":null}]}\n\n".to_vec()),
            Part::Bytes(b"data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n".to_vec()),
            Part::Bytes(b"data: [DONE]\n\n".to_vec()),
        ])
    })
    .await;
    let bodies = Arc::new(Bodies(Mutex::default(), 64 * 1024));
    let g = TestGateway::start_with_bodies(&yaml(&mock), bodies.clone()).await;
    let r = g.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    for _ in 0..100 {
        if bodies.0.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    let recs = bodies.0.lock().unwrap().clone();
    let resp = recs
        .iter()
        .find(|r| r.direction == "response")
        .expect("a response body");
    assert!(
        resp.body.contains("event: message_start")
            && resp.body.contains("streamed words")
            && resp.body.contains("message_stop"),
        "{}",
        resp.body
    );
    assert_eq!(
        resp.bytes,
        r.body.len(),
        "the logged size is what the client received"
    );
    assert!(!resp.truncated);
}

#[tokio::test]
async fn nothing_is_logged_when_it_is_off() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let g = TestGateway::start(&yaml(&mock)).await;
    assert!(!g.shared.bodies.enabled());
    assert_eq!(g.post("/v1/messages", &request(false)).await.status, 200);
}
