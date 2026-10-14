//! `POST /v1/embeddings`: routed like any other request, forwarded to OpenAI-protocol upstreams.

mod support;

use serde_json::{json, Value};
use support::{Mock, Reply, TestGateway};

fn reply() -> Value {
    json!({"object": "list", "data": [{"object": "embedding", "index": 0, "embedding": [0.25, -0.5, 0.125]}], "model": "text-embedding-3-small", "usage": {"prompt_tokens": 6, "total_tokens": 6}})
}

fn yaml(targets: &str, upstreams: &str) -> String {
    format!("server:\n  affinity: false\nupstreams:\n{upstreams}models:\n  - {{ match: \"e*\", targets: [ {targets} ] }}\n  - {{ match: \"my-embedder\", targets: [ {targets} ] }}\n")
}

#[tokio::test]
async fn embeddings_are_forwarded_with_the_model_mapped_and_named_back() {
    let mock = Mock::start(|_| Reply::json(200, reply())).await;
    let g = TestGateway::start(&yaml(
        "{ upstream: up, model: text-embedding-3-small }",
        &format!("  - {{ name: up, url: \"{}\" }}\n", mock.url()),
    ))
    .await;
    let r = g
        .post(
            "/v1/embeddings",
            &json!({"model": "my-embedder", "input": "hello there", "dimensions": 3}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(
        v["model"], "my-embedder",
        "the reply names the model the client asked for"
    );
    assert_eq!(v["data"][0]["embedding"], json!([0.25, -0.5, 0.125]));
    let seen = &mock.recorded()[0];
    assert_eq!(seen.path, "/v1/embeddings");
    assert_eq!(seen.json()["model"], "text-embedding-3-small");
    assert_eq!(seen.json()["dimensions"], 3);
    assert_eq!(r.header("x-airmate-upstream"), Some("up"));
}

#[tokio::test]
async fn an_upstream_without_an_embeddings_api_is_skipped_for_the_next_target() {
    let anthropic = Mock::start(|_| Reply::json(200, json!({}))).await;
    let openai = Mock::start(|_| Reply::json(200, reply())).await;
    let g = TestGateway::start(&yaml(
        "{ upstream: ant, model: claude-x }, { upstream: oa, model: text-embedding-3-small }",
        &format!(
            "  - {{ name: ant, url: \"{}\", protocol: anthropic }}\n  - {{ name: oa, url: \"{}\" }}\n",
            anthropic.url(),
            openai.url()
        ),
    ))
    .await;
    let r = g
        .post(
            "/v1/embeddings",
            &json!({"model": "e", "input": ["a", "b"]}),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(anthropic.count(), 0);
    assert_eq!(r.header("x-airmate-upstream"), Some("oa"));
    assert!(
        r.header("x-airmate-skipped")
            .unwrap_or("")
            .contains("ant/claude-x:no-embeddings"),
        "{:?}",
        r.headers
    );
}

#[tokio::test]
async fn when_no_target_can_embed_the_error_says_why() {
    let anthropic = Mock::start(|_| Reply::json(200, json!({}))).await;
    let g = TestGateway::start(&yaml(
        "{ upstream: ant, model: claude-x }",
        &format!(
            "  - {{ name: ant, url: \"{}\", protocol: anthropic }}\n",
            anthropic.url()
        ),
    ))
    .await;
    let r = g
        .post("/v1/embeddings", &json!({"model": "e", "input": "x"}))
        .await;
    assert_eq!(r.status, 400);
    assert!(
        r.json()["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no embeddings endpoint"),
        "{}",
        r.text()
    );
    assert_eq!(
        r.json()["error"]["type"],
        "invalid_request_error",
        "errors use the OpenAI envelope"
    );
}

#[tokio::test]
async fn bad_requests_are_refused_before_any_upstream_is_called() {
    let mock = Mock::start(|_| Reply::json(200, reply())).await;
    let g = TestGateway::start(&yaml(
        "{ upstream: up, model: m }",
        &format!("  - {{ name: up, url: \"{}\" }}\n", mock.url()),
    ))
    .await;
    for (body, field) in [
        (json!({"input": "x"}), "model"),
        (json!({"model": "e"}), "input"),
        (
            json!({"model": "e", "input": "x", "stream": true}),
            "stream",
        ),
    ] {
        let r = g.post("/v1/embeddings", &body).await;
        assert_eq!(r.status, 400, "{body}");
        assert!(
            r.json()["error"]["message"]
                .as_str()
                .unwrap()
                .contains(field),
            "{}",
            r.text()
        );
    }
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn embeddings_need_the_token_and_only_post_works() {
    let mock = Mock::start(|_| Reply::json(200, reply())).await;
    let g = TestGateway::start(&yaml(
        "{ upstream: up, model: m }",
        &format!("  - {{ name: up, url: \"{}\" }}\n", mock.url()),
    ))
    .await;
    let r = g
        .post_with(
            "/v1/embeddings",
            &json!({"model": "e", "input": "x"}),
            &[("x-api-key", "wrong")],
        )
        .await;
    assert_eq!(r.status, 401);
    assert_eq!(g.get("/v1/embeddings").await.status, 404);
}

#[tokio::test]
async fn embedding_usage_is_counted_in_the_log_and_metrics() {
    let mock = Mock::start(|_| Reply::json(200, reply())).await;
    let g = TestGateway::start(&yaml(
        "{ upstream: up, model: m }",
        &format!("  - {{ name: up, url: \"{}\" }}\n", mock.url()),
    ))
    .await;
    assert_eq!(
        g.post("/v1/embeddings", &json!({"model": "e", "input": "hello"}))
            .await
            .status,
        200
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let text = airmate::admin::metrics_text(&g.shared);
    assert!(
        text.contains("airmate_tokens_total{target=\"up/m\",kind=\"input\"} 6"),
        "{text}"
    );
}
