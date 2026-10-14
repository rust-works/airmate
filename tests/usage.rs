//! The usage ledger, prices and budgets, end to end against scripted upstreams.

mod support;

use std::sync::{Arc, Mutex};

use airmate::auth::hash_key;
use airmate::router::clock::{FixedRandom, ManualClock};
use airmate::usage::{UsageRecord, UsageSink};
use serde_json::{json, Value};
use support::{env_with_key, Capture, Mock, Reply, TestGateway};

#[derive(Default)]
struct Ledger(Mutex<Vec<UsageRecord>>);

impl UsageSink for Ledger {
    fn record(&self, rec: &UsageRecord) {
        self.0.lock().unwrap().push(rec.clone());
    }
}

impl Ledger {
    fn all(&self) -> Vec<UsageRecord> {
        self.0.lock().unwrap().clone()
    }
}

const DAY: u64 = 86_400;
// 2026-10-14 00:00:00 UTC.
const T0: u64 = 1_791_936_000;

fn chat_ok() -> Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}})
}

fn request(model: &str) -> Value {
    json!({"model": model, "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]})
}

/// Costs $0.007 per request: 5 input tokens at $1000/Mtok and 1 output token at $2000/Mtok.
fn data(json: &str) -> support::Part {
    support::Part::Bytes(format!("data: {json}\n\n").into_bytes())
}

const PRICE: &str = "price: { input_per_mtok: 1000, output_per_mtok: 2000 }";

struct Setup {
    g: TestGateway,
    clock: Arc<ManualClock>,
    ledger: Arc<Ledger>,
    log: Arc<Capture>,
}

async fn start(yaml: &str, history: &[UsageRecord]) -> Setup {
    let clock = Arc::new(ManualClock::new());
    clock.set_unix(T0 + 3600);
    let ledger = Arc::new(Ledger::default());
    let log = Arc::new(Capture::default());
    let g = TestGateway::start_metered(
        yaml,
        &env_with_key("UP_KEY", "k"),
        log.clone(),
        clock.clone(),
        Arc::new(FixedRandom::new(&[0.0])),
        ledger.clone(),
        history,
    )
    .await;
    Setup {
        g,
        clock,
        ledger,
        log,
    }
}

fn config(mock: &Mock, target_extra: &str, client_extra: &str, other: &str) -> String {
    format!(
        "server:\n  max_concurrent: 1000\n  affinity: false\nupstreams:\n  - {{ name: up, url: \"{url}\" }}\n  - {{ name: cheap, url: \"{url}\" }}\nmodels:\n  - {{ match: \"cheap-*\", name: cheap, targets: [ {{ upstream: cheap, model: free }} ] }}\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m, {target_extra} }} ] }}\nclients:\n  - name: ci\n    key_hash: \"{hash}\"\n{client_extra}{other}",
        url = mock.url(),
        hash = hash_key("ci-key"),
    )
}

async fn call(g: &TestGateway, model: &str) -> support::Resp {
    g.post_with("/v1/messages", &request(model), &[("x-api-key", "ci-key")])
        .await
}

#[tokio::test]
async fn a_priced_request_is_written_to_the_ledger_with_its_cost() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let s = start(&config(&mock, PRICE, "", ""), &[]).await;
    assert_eq!(call(&s.g, "claude-x").await.status, 200);
    let l = s.ledger.all();
    assert_eq!(l.len(), 1);
    let r = &l[0];
    assert_eq!(
        (
            r.client.as_str(),
            r.target.as_str(),
            r.upstream_model.as_str()
        ),
        ("ci", "up/m", "m")
    );
    assert_eq!((r.input_tokens, r.output_tokens, r.unix), (5, 1, T0 + 3600));
    assert!((r.cost_usd.unwrap() - 0.007).abs() < 1e-12);
    assert_eq!(r.ts, "2026-10-14T01:00:00.000Z");
    let reqs = s.log.wait_for(2).await;
    let request_rec = reqs
        .iter()
        .find(|r| r.kind == airmate::obs::RecordKind::Request)
        .unwrap();
    assert!(
        (request_rec.est_cost_usd.unwrap() - 0.007).abs() < 1e-12,
        "the request log carries the cost too"
    );
}

#[tokio::test]
async fn an_unpriced_model_has_unknown_cost_not_zero() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let s = start(&config(&mock, "", "", ""), &[]).await;
    assert_eq!(call(&s.g, "claude-x").await.status, 200);
    let l = s.ledger.all();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].cost_usd, None);
    let reqs = s.log.wait_for(2).await;
    assert!(reqs.iter().all(|r| r.est_cost_usd.is_none()));
}

#[tokio::test]
async fn a_price_file_prices_models_that_carry_no_price() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let dir = std::env::temp_dir().join(format!("airmate-prices-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("prices.yaml");
    std::fs::write(
        &file,
        "prices:\n  - { match: \"m\", input_per_mtok: 10, output_per_mtok: 20 }\n",
    )
    .unwrap();
    let s = start(
        &config(
            &mock,
            "",
            "",
            &format!("usage:\n  prices: '{}'\n", file.display()),
        ),
        &[],
    )
    .await;
    assert_eq!(call(&s.g, "claude-x").await.status, 200);
    assert!((s.ledger.all()[0].cost_usd.unwrap() - 0.00007).abs() < 1e-12);
    // The target's own price wins over the file.
    let s2 = start(
        &config(
            &mock,
            PRICE,
            "",
            &format!("usage:\n  prices: '{}'\n", file.display()),
        ),
        &[],
    )
    .await;
    assert_eq!(call(&s2.g, "claude-x").await.status, 200);
    assert!((s2.ledger.all()[0].cost_usd.unwrap() - 0.007).abs() < 1e-12);
}

#[tokio::test]
async fn an_exhausted_budget_is_a_429_until_the_period_renews() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let s = start(
        &config(&mock, PRICE, "    budget: { usd_per_day: 0.01 }\n", ""),
        &[],
    )
    .await;
    assert_eq!(call(&s.g, "claude-x").await.status, 200, "$0.007 of $0.01");
    assert_eq!(
        call(&s.g, "claude-x").await.status,
        200,
        "the check is before the call, so this one runs and tips it over"
    );
    let denied = call(&s.g, "claude-x").await;
    assert_eq!(denied.status, 429, "{}", denied.text());
    assert_eq!(denied.json()["error"]["type"], "rate_limit_error");
    let msg = denied.json()["error"]["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        msg.contains("budget exhausted")
            && msg.contains("`ci`")
            && msg.contains("daily")
            && msg.contains("$0.01")
            && msg.contains("2026-10-15T00:00:00.000Z"),
        "{msg}"
    );
    assert_eq!(
        denied.header("retry-after"),
        Some("82800"),
        "23 hours to midnight"
    );
    assert_eq!(
        mock.count(),
        2,
        "nothing was sent upstream for the denied request"
    );

    // Other clients are unaffected, and the budget renews at midnight UTC.
    assert_eq!(
        s.g.post("/v1/messages", &request("claude-x")).await.status,
        200
    );
    s.clock.set_unix(T0 + DAY + 5);
    assert_eq!(call(&s.g, "claude-x").await.status, 200);
}

#[tokio::test]
async fn unpriced_requests_do_not_spend_a_budget() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let s = start(
        &config(&mock, "", "    budget: { usd_per_day: 0.001 }\n", ""),
        &[],
    )
    .await;
    for _ in 0..4 {
        assert_eq!(call(&s.g, "claude-x").await.status, 200);
    }
}

#[tokio::test]
async fn a_budget_fallback_routes_to_the_named_entry_instead_of_refusing() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let extra = "    budget: { usd_per_day: 0.005 }\n    budget_fallback: cheap\n";
    let s = start(&config(&mock, PRICE, extra, ""), &[]).await;
    assert_eq!(call(&s.g, "claude-x").await.status, 200);
    let r = call(&s.g, "claude-x").await;
    assert_eq!(r.status, 200, "{}", r.text());
    let l = s.ledger.all();
    assert_eq!(l[0].target, "up/m");
    assert_eq!(
        l[1].target, "cheap/free",
        "past the budget the request goes to the fallback entry"
    );
    assert_eq!(l[1].cost_usd, None, "the fallback target is unpriced");
}

#[tokio::test]
async fn budgets_start_from_the_ledger_that_was_already_there() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let history = vec![
        UsageRecord {
            client: "ci".into(),
            unix: T0 + 60,
            cost_usd: Some(0.01),
            ..UsageRecord::default()
        },
        UsageRecord {
            client: "ci".into(),
            unix: T0 - 3600,
            cost_usd: Some(50.0),
            ..UsageRecord::default()
        },
    ];
    let s = start(
        &config(&mock, PRICE, "    budget: { usd_per_day: 0.01 }\n", ""),
        &history,
    )
    .await;
    let r = call(&s.g, "claude-x").await;
    assert_eq!(
        r.status,
        429,
        "today's recorded spend already reaches the cap; yesterday's does not count: {}",
        r.text()
    );
    assert_eq!(mock.count(), 0);
}

#[tokio::test]
async fn a_streamed_request_is_metered_when_the_stream_ends() {
    let mock = Mock::start(|_| {
        Reply::sse(vec![
            data(r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}"#),
            data(r#"{"id":"c","model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1}}"#),
            data("[DONE]"),
        ])
    })
    .await;
    let s = start(&config(&mock, PRICE, "", ""), &[]).await;
    let mut body = request("claude-x");
    body["stream"] = json!(true);
    let r =
        s.g.post_with("/v1/messages", &body, &[("x-api-key", "ci-key")])
            .await;
    assert_eq!(r.status, 200, "{}", r.text());
    for _ in 0..100 {
        if !s.ledger.all().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    let l = s.ledger.all();
    assert_eq!(l.len(), 1);
    assert_eq!((l[0].input_tokens, l[0].output_tokens), (5, 1));
    assert!((l[0].cost_usd.unwrap() - 0.007).abs() < 1e-12);
}
