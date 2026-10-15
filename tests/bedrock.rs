//! Bedrock as an upstream: Converse and ConverseStream, signed with Signature Version 4.

mod support;

use airmate_core::eventstream::encode;
use airmate_core::outbound::sigv4::{sign, Credentials, Request as SignRequest};
use serde_json::{json, Value};
use support::{Mock, Part, Recorded, Reply, TestGateway};

const MODEL: &str = "anthropic.claude-3-5-sonnet-20241022-v2:0";
const AK: &str = "AKIAEXAMPLEKEYID123";
const SK: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

fn env(n: &str) -> Option<String> {
    match n {
        "AK" => Some(AK.to_owned()),
        "SK" => Some(SK.to_owned()),
        "ST" => Some("session-token-value-xyz".to_owned()),
        "BEDROCK_KEY" => Some("bedrock-api-key-secret-1".to_owned()),
        _ => None,
    }
}

fn yaml(mock: &Mock, auth: &str) -> String {
    format!(
        "server:\n  affinity: false\nupstreams:\n  - {{ name: b, url: \"{}\", protocol: bedrock, {auth} }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: b, model: \"{MODEL}\" }} ] }}\n",
        mock.url()
    )
}

const SIGNED: &str = "aws: { region: us-east-1, access_key_id: { env: AK }, secret_access_key: { env: SK }, session_token: { env: ST } }";

fn converse_reply(blocks: Value, stop: &str) -> Value {
    json!({"output": {"message": {"role": "assistant", "content": blocks}}, "stopReason": stop,
        "usage": {"inputTokens": 11, "outputTokens": 5, "totalTokens": 16}, "metrics": {"latencyMs": 50}})
}

fn request(stream: bool) -> Value {
    json!({"model": "claude-x", "max_tokens": 64, "stream": stream, "system": "be brief", "messages": [{"role": "user", "content": "hello"}]})
}

async fn start(mock: &Mock, auth: &str) -> TestGateway {
    TestGateway::start_with_env(&yaml(mock, auth), &env).await
}

/// Recomputes the signature from what the mock actually received.
fn expected_authorization(r: &Recorded) -> String {
    let host = r.header("host").expect("a host header");
    let date = r.header("x-amz-date").expect("an x-amz-date header");
    let signed = sign(
        &Credentials {
            access_key_id: AK,
            secret_access_key: SK,
            session_token: Some("session-token-value-xyz"),
        },
        &SignRequest {
            method: "POST",
            host,
            path: &r.path,
            query: &r.query,
            body: &r.body,
            region: "us-east-1",
            service: "bedrock",
            amz_date: date,
        },
    );
    signed
        .into_iter()
        .find(|(k, _)| k == "authorization")
        .unwrap()
        .1
}

#[tokio::test]
async fn a_signed_converse_call_carries_a_signature_that_verifies() {
    let mock = Mock::start(|_| {
        Reply::json(
            200,
            converse_reply(json!([{"text": "hi from bedrock"}]), "end_turn"),
        )
    })
    .await;
    let g = start(&mock, SIGNED).await;
    let r = g.post("/v1/messages", &request(false)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["content"][0]["text"], "hi from bedrock");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(
        (
            v["usage"]["input_tokens"].as_u64(),
            v["usage"]["output_tokens"].as_u64()
        ),
        (Some(11), Some(5))
    );

    let seen = &mock.recorded()[0];
    assert_eq!(
        seen.path,
        "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse"
    );
    let auth = seen.header("authorization").unwrap();
    assert!(
        auth.starts_with(&format!("AWS4-HMAC-SHA256 Credential={AK}/"))
            && auth.contains("/us-east-1/bedrock/aws4_request"),
        "{auth}"
    );
    assert!(
        auth.contains("SignedHeaders=host;x-amz-date;x-amz-security-token"),
        "{auth}"
    );
    assert_eq!(
        auth,
        expected_authorization(seen),
        "the signature matches the request as received"
    );
    assert_eq!(
        seen.header("x-amz-security-token"),
        Some("session-token-value-xyz")
    );
    assert!(!auth.contains(SK), "the secret key never goes on the wire");
    let b = seen.json();
    assert_eq!(b["system"], json!([{"text": "be brief"}]));
    assert_eq!(
        b["messages"][0],
        json!({"role": "user", "content": [{"text": "hello"}]})
    );
    assert_eq!(b["inferenceConfig"]["maxTokens"], 64);
}

#[tokio::test]
async fn a_bedrock_api_key_is_sent_as_a_bearer_token() {
    let mock =
        Mock::start(|_| Reply::json(200, converse_reply(json!([{"text": "ok"}]), "end_turn")))
            .await;
    let g = start(&mock, "api_key: { env: BEDROCK_KEY }").await;
    assert_eq!(g.post("/v1/messages", &request(false)).await.status, 200);
    let seen = &mock.recorded()[0];
    assert_eq!(
        seen.header("authorization"),
        Some("Bearer bedrock-api-key-secret-1")
    );
    assert!(seen.header("x-amz-date").is_none());
}

#[tokio::test]
async fn a_converse_stream_is_decoded_and_translated() {
    let mock = Mock::start(|_| {
        let ev = |kind: &str, j: Value| {
            Part::Bytes(encode(
                &[
                    (":message-type", "event"),
                    (":event-type", kind),
                    (":content-type", "application/json"),
                ],
                j.to_string().as_bytes(),
            ))
        };
        Reply::Stream {
            status: 200,
            headers: vec![(
                "content-type".into(),
                "application/vnd.amazon.eventstream".into(),
            )],
            parts: vec![
                ev("messageStart", json!({"role": "assistant"})),
                ev(
                    "contentBlockDelta",
                    json!({"contentBlockIndex": 0, "delta": {"text": "Hel"}}),
                ),
                ev(
                    "contentBlockDelta",
                    json!({"contentBlockIndex": 0, "delta": {"text": "lo"}}),
                ),
                ev("contentBlockStop", json!({"contentBlockIndex": 0})),
                ev("messageStop", json!({"stopReason": "end_turn"})),
                ev(
                    "metadata",
                    json!({"usage": {"inputTokens": 4, "outputTokens": 2}}),
                ),
            ],
        }
    })
    .await;
    let g = start(&mock, SIGNED).await;
    let r = g.post("/v1/messages", &request(true)).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let t = r.text();
    assert!(
        t.contains("\"text\":\"Hel\"")
            && t.contains("\"text\":\"lo\"")
            && t.contains("message_stop"),
        "{t}"
    );
    let seen = &mock.recorded()[0];
    assert_eq!(
        seen.path,
        "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse-stream"
    );
    assert_eq!(
        seen.header("accept"),
        Some("application/vnd.amazon.eventstream")
    );
    assert_eq!(
        seen.header("authorization").unwrap(),
        expected_authorization(seen)
    );
}

#[tokio::test]
async fn a_mid_stream_exception_reaches_the_client_as_an_error_event() {
    let mock = Mock::start(|_| {
        let ev = |kind: &str, j: Value| {
            Part::Bytes(encode(
                &[(":message-type", "event"), (":event-type", kind)],
                j.to_string().as_bytes(),
            ))
        };
        Reply::Stream {
            status: 200,
            headers: vec![],
            parts: vec![
                ev("messageStart", json!({"role": "assistant"})),
                ev(
                    "contentBlockDelta",
                    json!({"contentBlockIndex": 0, "delta": {"text": "par"}}),
                ),
                Part::Bytes(encode(
                    &[
                        (":message-type", "exception"),
                        (":exception-type", "modelStreamErrorException"),
                    ],
                    br#"{"message":"the model failed"}"#,
                )),
            ],
        }
    })
    .await;
    let g = start(&mock, SIGNED).await;
    let t = g.post("/v1/messages", &request(true)).await.text();
    assert!(
        t.contains("event: error") && t.contains("the model failed"),
        "{t}"
    );
    assert!(!t.contains("message_stop"), "{t}");
}

#[tokio::test]
async fn tool_use_round_trips() {
    let mock = Mock::start(|r| {
        if r.json()["messages"].as_array().unwrap().len() == 1 {
            Reply::json(200, converse_reply(json!([{"toolUse": {"toolUseId": "tu_1", "name": "get_weather", "input": {"city": "Paris"}}}]), "tool_use"))
        } else {
            Reply::json(200, converse_reply(json!([{"text": "sunny"}]), "end_turn"))
        }
    })
    .await;
    let g = start(&mock, SIGNED).await;
    let mut req = json!({"model": "claude-x", "max_tokens": 64,
        "tools": [{"name": "get_weather", "description": "w", "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}],
        "messages": [{"role": "user", "content": "weather?"}]});
    let first = g.post("/v1/messages", &req).await.json();
    assert_eq!(first["stop_reason"], "tool_use");
    let call = first["content"][0].clone();
    assert_eq!(
        (call["id"].as_str(), call["name"].as_str()),
        (Some("tu_1"), Some("get_weather"))
    );
    req["messages"] = json!([
        {"role": "user", "content": "weather?"},
        {"role": "assistant", "content": [call]},
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tu_1", "content": "sunny"}]}
    ]);
    let second = g.post("/v1/messages", &req).await;
    assert_eq!(second.json()["content"][0]["text"], "sunny");
    let sent = mock.recorded()[1].json();
    assert_eq!(
        sent["messages"][2]["content"][0]["toolResult"],
        json!({"toolUseId": "tu_1", "content": [{"text": "sunny"}], "status": "success"})
    );
}

#[tokio::test]
async fn bedrock_errors_keep_their_meaning_and_nothing_secret_leaks() {
    let mock = Mock::start(|_| {
        Reply::json(
            429,
            json!({"message": "Too many requests, please wait before trying again."}),
        )
    })
    .await;
    let g = start(&mock, SIGNED).await;
    let r = g.post("/v1/messages", &request(false)).await;
    assert_eq!(r.status, 429, "{}", r.text());
    assert_eq!(r.json()["error"]["type"], "rate_limit_error");
    let long = Mock::start(|_| {
        Reply::json(
            400,
            json!({"message": "Input is too long for requested model."}),
        )
    })
    .await;
    let g2 = start(&long, SIGNED).await;
    let r = g2.post("/v1/messages", &request(false)).await;
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("prompt is too long"),
        "{}",
        r.text()
    );
    for text in [
        airmate::admin::metrics_text(&g.shared),
        airmate::admin::status_json(&g.shared).to_string(),
    ] {
        for secret in [AK, SK, "session-token-value-xyz"] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
    }
}

#[tokio::test]
async fn structured_output_is_refused_because_converse_cannot_honour_it() {
    let mock =
        Mock::start(|_| Reply::json(200, converse_reply(json!([{"text": "x"}]), "end_turn"))).await;
    let g = start(&mock, SIGNED).await;
    let r = g
        .post("/v1/chat/completions", &json!({"model": "gpt-x", "messages": [{"role": "user", "content": "hi"}], "response_format": {"type": "json_object"}}))
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("structured output"),
        "{}",
        r.text()
    );
    assert_eq!(mock.count(), 0);
}
