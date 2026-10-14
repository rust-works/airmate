//! Named clients: their keys, model access and rate limits.

mod support;

use std::sync::Arc;
use std::time::Duration;

use airmate::auth::hash_key;
use airmate::router::clock::{FixedRandom, ManualClock};
use serde_json::{json, Value};
use support::{env_with_key, Capture, Mock, Reply, TestGateway};

fn chat_ok(prompt_tokens: u64) -> Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": 1}})
}

fn request(model: &str, text: &str) -> Value {
    json!({"model": model, "max_tokens": 8, "messages": [{"role": "user", "content": text}]})
}

fn yaml(mock: &Mock, clients: &str, extra: &str) -> String {
    format!(
        "server:\n  max_concurrent: 1000\n  affinity: false\nupstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\nclients:\n{clients}{extra}",
        mock.url()
    )
}

fn client(name: &str, key: &str, rest: &str) -> String {
    format!(
        "  - name: {name}\n    key_hash: \"{}\"\n{rest}",
        hash_key(key)
    )
}

async fn start(yaml: &str) -> (TestGateway, Arc<ManualClock>, Arc<Capture>) {
    let clock = Arc::new(ManualClock::new());
    let cap = Arc::new(Capture::default());
    let g = TestGateway::start_full(
        yaml,
        &env_with_key("UP_KEY", "k"),
        cap.clone(),
        clock.clone(),
        Arc::new(FixedRandom::new(&[0.0])),
    )
    .await;
    (g, clock, cap)
}

async fn as_client(g: &TestGateway, key: &str, body: &Value) -> support::Resp {
    g.post_with("/v1/messages", body, &[("x-api-key", key)])
        .await
}

#[tokio::test]
async fn a_client_key_authenticates_and_is_named_in_the_log() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let (g, _, cap) = start(&yaml(&mock, &client("ci", "am_ci_key", ""), "")).await;
    let r = as_client(&g, "am_ci_key", &request("claude-x", "hi")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        as_client(&g, "am_wrong", &request("claude-x", "hi"))
            .await
            .status,
        401
    );
    assert_eq!(
        g.post("/v1/messages", &request("claude-x", "hi"))
            .await
            .status,
        200,
        "the gateway token still works"
    );
    let recs = cap.wait_for(4).await;
    let clients: Vec<_> = recs
        .iter()
        .filter(|r| r.kind == airmate::obs::RecordKind::Request)
        .map(|r| r.client.as_str())
        .collect();
    assert!(
        clients.contains(&"ci") && clients.contains(&"default") && clients.contains(&"-"),
        "{clients:?}"
    );
    assert!(
        !serde_json::to_string(&recs).unwrap().contains("am_ci_key"),
        "keys are never logged"
    );
}

#[tokio::test]
async fn a_client_may_only_use_the_models_it_is_allowed() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let c = client("laptop", "k1", "    models: [\"claude-haiku-*\"]\n");
    let (g, _, _) = start(&yaml(&mock, &c, "")).await;
    assert_eq!(
        as_client(&g, "k1", &request("claude-haiku-4", "hi"))
            .await
            .status,
        200
    );
    let denied = as_client(&g, "k1", &request("claude-opus-4", "hi")).await;
    assert_eq!(denied.status, 403);
    assert_eq!(denied.json()["error"]["type"], "permission_error");
    assert!(
        denied.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("`laptop`")
            && denied.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("claude-opus-4")
    );
    assert_eq!(mock.count(), 1, "a denied request reaches no upstream");
}

#[tokio::test]
async fn the_request_limit_is_a_429_with_retry_after_and_recovers_with_time() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let c = client("ci", "k1", "    rate: { rpm: 3 }\n");
    let (g, clock, _) = start(&yaml(&mock, &c, "")).await;
    for _ in 0..3 {
        assert_eq!(
            as_client(&g, "k1", &request("claude-x", "hi")).await.status,
            200
        );
    }
    let limited = as_client(&g, "k1", &request("claude-x", "hi")).await;
    assert_eq!(limited.status, 429);
    assert_eq!(limited.json()["error"]["type"], "rate_limit_error");
    assert!(limited.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("requests per minute"));
    assert_eq!(
        limited.header("retry-after"),
        Some("21"),
        "3 a minute is one per 20 s, rounded up"
    );
    assert_eq!(mock.count(), 3);
    clock.advance(Duration::from_secs(21));
    assert_eq!(
        as_client(&g, "k1", &request("claude-x", "hi")).await.status,
        200
    );
    // Another client is unaffected.
    assert_eq!(
        g.post("/v1/messages", &request("claude-x", "hi"))
            .await
            .status,
        200
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_limit_is_exact_under_concurrent_load() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let c = client("ci", "k1", "    rate: { rpm: 10 }\n");
    let (g, _, _) = start(&yaml(&mock, &c, "")).await;
    let g = Arc::new(g);
    let tasks: Vec<_> = (0..60)
        .map(|_| {
            let g = g.clone();
            tokio::spawn(
                async move { as_client(&g, "k1", &request("claude-x", "hi")).await.status },
            )
        })
        .collect();
    let mut ok = 0;
    let mut limited = 0;
    for t in tasks {
        match t.await.unwrap() {
            200 => ok += 1,
            429 => limited += 1,
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(
        (ok, limited),
        (10, 50),
        "exactly the limit, however the requests interleave"
    );
    assert_eq!(mock.count(), 10);
}

#[tokio::test]
async fn the_token_limit_uses_the_estimate_up_front_and_settles_with_true_usage() {
    // The prompt is about 1,000 characters (250 tokens plus overhead) but the model reports 5.
    let mock = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let c = client("ci", "k1", "    rate: { tpm: 600 }\n");
    let (g, _, _) = start(&yaml(&mock, &c, "")).await;
    let big = request("claude-x", &"x".repeat(1000));
    assert_eq!(as_client(&g, "k1", &big).await.status, 200);
    // Without settlement the bucket would hold ~346 tokens and refuse a second big request.
    assert_eq!(
        as_client(&g, "k1", &big).await.status,
        200,
        "the first request used 5 tokens, so the rest was given back"
    );
    assert_eq!(as_client(&g, "k1", &big).await.status, 200);
}

#[tokio::test]
async fn an_underestimate_is_charged_and_the_next_request_waits() {
    // The model reports far more input than the estimate said.
    let mock = Mock::start(|_| Reply::json(200, chat_ok(5000))).await;
    let c = client("ci", "k1", "    rate: { tpm: 600 }\n");
    let (g, _, _) = start(&yaml(&mock, &c, "")).await;
    assert_eq!(
        as_client(&g, "k1", &request("claude-x", "short"))
            .await
            .status,
        200
    );
    let next = as_client(&g, "k1", &request("claude-x", "short")).await;
    assert_eq!(next.status, 429, "{}", next.text());
    assert!(next.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("input tokens per minute"));
}

#[tokio::test]
async fn an_upstream_rate_limit_is_a_skip_so_fallback_absorbs_it() {
    let a = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let b = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let y = format!(
        "server:\n  affinity: false\nupstreams:\n  - {{ name: a, url: \"{}\", rate: {{ rpm: 2 }} }}\n  - {{ name: b, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: a, model: m }}, {{ upstream: b, model: m }} ] }}\n",
        a.url(),
        b.url()
    );
    let (g, _, _) = start(&y).await;
    for _ in 0..2 {
        assert_eq!(
            g.post("/v1/messages", &request("claude-x", "hi"))
                .await
                .header("x-airmate-upstream"),
            Some("a")
        );
    }
    let third = g.post("/v1/messages", &request("claude-x", "hi")).await;
    assert_eq!(
        third.status,
        200,
        "an upstream limit is not an error: {}",
        third.text()
    );
    assert_eq!(third.header("x-airmate-upstream"), Some("b"));
    assert_eq!(third.header("x-airmate-skipped"), Some("a/m:rate-limit"));
    assert_eq!(a.count(), 2);
}

#[tokio::test]
async fn a_revoked_key_is_refused_after_reload() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let both = format!(
        "{}{}",
        client("ci", "k_ci", ""),
        client("laptop", "k_laptop", "")
    );
    let (g, _, _) = start(&yaml(&mock, &both, "")).await;
    assert_eq!(
        as_client(&g, "k_laptop", &request("claude-x", "hi"))
            .await
            .status,
        200
    );
    g.reload(&yaml(&mock, &client("ci", "k_ci", ""), ""))
        .unwrap();
    assert_eq!(
        as_client(&g, "k_laptop", &request("claude-x", "hi"))
            .await
            .status,
        401,
        "revocation takes effect on reload"
    );
    assert_eq!(
        as_client(&g, "k_ci", &request("claude-x", "hi"))
            .await
            .status,
        200
    );
}

#[tokio::test]
async fn limits_survive_a_reload_unless_the_limit_itself_changes() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok(5))).await;
    let c = client("ci", "k1", "    rate: { rpm: 2 }\n");
    let (g, _, _) = start(&yaml(&mock, &c, "")).await;
    for _ in 0..2 {
        assert_eq!(
            as_client(&g, "k1", &request("claude-x", "hi")).await.status,
            200
        );
    }
    g.reload(&yaml(&mock, &c, "")).unwrap();
    assert_eq!(
        as_client(&g, "k1", &request("claude-x", "hi")).await.status,
        429,
        "a reload does not hand out a fresh allowance"
    );
    g.reload(&yaml(
        &mock,
        &client("ci", "k1", "    rate: { rpm: 5 }\n"),
        "",
    ))
    .unwrap();
    assert_eq!(
        as_client(&g, "k1", &request("claude-x", "hi")).await.status,
        200,
        "a changed limit starts a fresh bucket"
    );
}
