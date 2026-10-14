//! The mock-upstream conformance suite for the router: failover, "no failover after the first
//! byte", breaker transitions, backoff, `Retry-After`, strategies and affinity, driven against
//! scripted upstreams and (where time matters) an injected clock.

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use airmate::router::clock::{Clock, FixedRandom, ManualClock};
use serde_json::{json, Value};
use support::{env_with_key, Capture, Mock, Part, Reply, TestGateway};

fn chat_ok(text: &str) -> Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
}

fn err(msg: &str) -> Value {
    json!({"error": {"message": msg}})
}

fn request() -> Value {
    json!({"model": "claude-x", "max_tokens": 16, "messages": [{"role": "user", "content": "hello there"}]})
}

fn stream_request() -> Value {
    json!({"model": "claude-x", "max_tokens": 16, "stream": true, "messages": [{"role": "user", "content": "hello there"}]})
}

/// A config with upstreams `a`, `b`, `c` (those with a URL), and a fallback entry over them.
fn config(urls: &[(&str, &str)], extra_upstream: &str, extra_top: &str) -> String {
    let mut y = String::from("upstreams:\n");
    for (name, url) in urls {
        y.push_str(&format!(
            "  - name: {name}\n    url: {url}\n    api_key: {{ env: UP_KEY }}\n{extra_upstream}"
        ));
    }
    y.push_str("models:\n  - match: \"claude-*\"\n    name: claude\n    targets:\n");
    for (name, _) in urls {
        y.push_str(&format!("      - {{ upstream: {name}, model: m }}\n"));
    }
    // Affinity would pull a repeated conversation to the target that served it, which hides the
    // behaviour most tests are about; it is off unless a test turns it on.
    if !extra_top.contains("server:") {
        y.push_str("server:\n  affinity: false\n");
    }
    y.push_str(extra_top);
    y
}

async fn gw(yaml: &str) -> TestGateway {
    TestGateway::start_with_env(yaml, &env_with_key("UP_KEY", "k")).await
}

async fn gw_clock(yaml: &str) -> (TestGateway, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new());
    let g = TestGateway::start_full(
        yaml,
        &env_with_key("UP_KEY", "k"),
        Arc::new(airmate::obs::NullSink),
        clock.clone(),
        Arc::new(FixedRandom::new(&[0.0])),
    )
    .await;
    (g, clock)
}

fn counting(
    status: u16,
    body: Value,
    hits: &Arc<AtomicUsize>,
) -> impl Fn(&support::Recorded) -> Reply + Send + Sync + 'static {
    let hits = hits.clone();
    move |_| {
        hits.fetch_add(1, Ordering::SeqCst);
        Reply::json(status, body.clone())
    }
}

#[tokio::test]
async fn a_retryable_failure_fails_over_to_the_next_target() {
    let bad = Mock::start(|_| Reply::json(503, err("busy"))).await;
    let good = Mock::start(|_| Reply::json(200, chat_ok("from b"))).await;
    let g = gw(&config(&[("a", &bad.url()), ("b", &good.url())], "", "")).await;
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["content"][0]["text"], "from b");
    assert_eq!(r.header("x-airmate-attempts"), Some("2"));
    assert_eq!(r.header("x-airmate-upstream"), Some("b"));
    assert_eq!((bad.count(), good.count()), (1, 1));
}

#[tokio::test]
async fn every_retryable_class_fails_over() {
    for (status, label) in [
        (408, "timeout"),
        (429, "rate limit"),
        (500, "server error"),
        (502, "bad gateway"),
        (503, "overloaded"),
        (529, "overloaded"),
    ] {
        let bad = Mock::start(move |_| Reply::json(status, err(label))).await;
        let good = Mock::start(|_| Reply::json(200, chat_ok("ok"))).await;
        let g = gw(&config(&[("a", &bad.url()), ("b", &good.url())], "", "")).await;
        let r = g.post("/v1/messages", &request()).await;
        assert_eq!(r.status, 200, "{status}: {}", r.text());
        assert_eq!(r.header("x-airmate-attempts"), Some("2"), "{status}");
    }
}

#[tokio::test]
async fn a_connection_error_and_a_silent_upstream_fail_over_too() {
    let good = Mock::start(|_| Reply::json(200, chat_ok("ok"))).await;
    let g = gw(&config(
        &[("a", "http://127.0.0.1:1"), ("b", &good.url())],
        "",
        "",
    ))
    .await;
    assert_eq!(
        g.post("/v1/messages", &request())
            .await
            .header("x-airmate-attempts"),
        Some("2")
    );
    let slow = Mock::start(|_| Reply::Slow(Duration::from_secs(30))).await;
    let yaml = config(
        &[("a", &slow.url()), ("b", &good.url())],
        "    timeouts: { connect_secs: 2, first_byte_secs: 1, idle_secs: 5 }\n",
        "",
    );
    let g = gw(&yaml).await;
    let started = Instant::now();
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.status, 200);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn a_non_retryable_failure_is_returned_at_once_and_never_tried_elsewhere() {
    for status in [400, 404, 413, 422] {
        let hits_b = Arc::new(AtomicUsize::new(0));
        let bad = Mock::start(move |_| Reply::json(status, err("your request is wrong"))).await;
        let other = Mock::start(counting(200, chat_ok("x"), &hits_b)).await;
        let g = gw(&config(&[("a", &bad.url()), ("b", &other.url())], "", "")).await;
        let r = g.post("/v1/messages", &request()).await;
        assert_eq!(
            r.status,
            if status == 422 { 400 } else { status },
            "{status}"
        );
        assert_eq!(r.header("x-airmate-attempts"), Some("1"));
        assert_eq!(
            hits_b.load(Ordering::SeqCst),
            0,
            "{status}: a bad request does not become good by trying another provider"
        );
    }
}

#[tokio::test]
async fn when_every_target_fails_the_last_error_is_returned() {
    let a = Mock::start(|_| Reply::json(503, err("a busy"))).await;
    let b = Mock::start(|_| Reply::json(429, err("b limited"))).await;
    let g = gw(&config(&[("a", &a.url()), ("b", &b.url())], "", "")).await;
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.status, 429);
    assert_eq!(r.json()["error"]["message"], "b limited");
    assert_eq!(r.header("x-airmate-attempts"), Some("2"));
}

#[tokio::test]
async fn the_attempt_cap_stops_the_search() {
    let hits = Arc::new(AtomicUsize::new(0));
    let mocks: Vec<Mock> = {
        let mut v = Vec::new();
        for _ in 0..4 {
            v.push(Mock::start(counting(500, err("boom"), &hits)).await);
        }
        v
    };
    let urls: Vec<(String, String)> = mocks
        .iter()
        .enumerate()
        .map(|(i, m)| (format!("u{i}"), m.url()))
        .collect();
    let pairs: Vec<(&str, &str)> = urls.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let g = gw(&config(&pairs, "", "failover:\n  max_attempts: 2\n")).await;
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.status, 500);
    assert_eq!(hits.load(Ordering::SeqCst), 2, "only two attempts are made");
    assert_eq!(r.header("x-airmate-attempts"), Some("2"));
}

#[tokio::test]
async fn retries_on_the_same_target_wait_with_backoff_before_moving_on() {
    let hits = Arc::new(AtomicUsize::new(0));
    let bad = Mock::start(counting(503, err("busy"), &hits)).await;
    let good = Mock::start(|_| Reply::json(200, chat_ok("ok"))).await;
    let top = "failover:\n  retries: 2\n  backoff_base_ms: 100\n  backoff_cap_ms: 100\n";
    let (g, _) = gw_clock(&config(&[("a", &bad.url()), ("b", &good.url())], "", top)).await;
    // The injected random source always rolls 0, so the full-jitter wait is zero here.
    let started = Instant::now();
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.status, 200);
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "the first try plus two retries"
    );
    assert_eq!(r.header("x-airmate-attempts"), Some("4"));
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn full_jitter_backoff_waits_up_to_the_cap() {
    let hits = Arc::new(AtomicUsize::new(0));
    let bad = Mock::start(counting(503, err("busy"), &hits)).await;
    let good = Mock::start(|_| Reply::json(200, chat_ok("ok"))).await;
    let top = "failover:\n  retries: 1\n  backoff_base_ms: 400\n  backoff_cap_ms: 400\n";
    let clock = Arc::new(ManualClock::new());
    // A roll of 0.9 waits 90% of the 400 ms ceiling.
    let g = TestGateway::start_full(
        &config(&[("a", &bad.url()), ("b", &good.url())], "", top),
        &env_with_key("UP_KEY", "k"),
        Arc::new(airmate::obs::NullSink),
        clock,
        Arc::new(FixedRandom::new(&[0.9])),
    )
    .await;
    let started = Instant::now();
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.status, 200);
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(300) && took < Duration::from_millis(900),
        "{took:?}"
    );
}

#[tokio::test]
async fn retry_after_within_the_ceiling_is_waited_for_on_the_same_target() {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let limited = Mock::start(move |_| {
        if seen.fetch_add(1, Ordering::SeqCst) == 0 {
            Reply::json_with(429, err("slow down"), &[("retry-after", "1")])
        } else {
            Reply::json(200, chat_ok("after the wait"))
        }
    })
    .await;
    let other = Mock::start(|_| Reply::json(200, chat_ok("other"))).await;
    let g = gw(&config(
        &[("a", &limited.url()), ("b", &other.url())],
        "",
        "failover:\n  retries: 1\n  retry_after_ceiling_secs: 5\n",
    ))
    .await;
    let started = Instant::now();
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.json()["content"][0]["text"], "after the wait");
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "the Retry-After was honoured: {:?}",
        started.elapsed()
    );
    assert_eq!(other.count(), 0);
}

#[tokio::test]
async fn retry_after_over_the_ceiling_marks_the_target_cooling_down_and_fails_over() {
    let limited =
        Mock::start(|_| Reply::json_with(429, err("slow down"), &[("retry-after", "30")])).await;
    let other = Mock::start(|_| Reply::json(200, chat_ok("other"))).await;
    let (g, clock) = gw_clock(&config(
        &[("a", &limited.url()), ("b", &other.url())],
        "",
        "failover:\n  retries: 1\n  retry_after_ceiling_secs: 2\n",
    ))
    .await;
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!((r.status, r.header("x-airmate-upstream")), (200, Some("b")));
    let again = g.post("/v1/messages", &request()).await;
    assert_eq!(
        again.header("x-airmate-skipped"),
        Some("a/m:cooling-down"),
        "{:?}",
        again.headers
    );
    assert_eq!(limited.count(), 1, "the cooling target is left alone");
    clock.advance(Duration::from_secs(31));
    g.post("/v1/messages", &request()).await;
    assert_eq!(limited.count(), 2, "after the pause it is tried again");
}

#[tokio::test]
async fn a_rejected_key_fails_over_opens_the_circuit_and_raises_an_alert() {
    let rejecting = Mock::start(|_| Reply::json(401, err("bad key"))).await;
    let other = Mock::start(|_| Reply::json(200, chat_ok("other"))).await;
    let (g, clock) = gw_clock(&config(
        &[("a", &rejecting.url()), ("b", &other.url())],
        "",
        "",
    ))
    .await;
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!((r.status, r.header("x-airmate-upstream")), (200, Some("b")));
    assert_eq!(g.shared.router.auth_alerts.load(Ordering::SeqCst), 1);
    let again = g.post("/v1/messages", &request()).await;
    assert_eq!(again.header("x-airmate-skipped"), Some("a/m:circuit-open"));
    assert_eq!(
        rejecting.count(),
        1,
        "the circuit protects a wrong key from being hammered"
    );
    clock.advance(Duration::from_secs(40));
    g.post("/v1/messages", &request()).await;
    assert_eq!(
        rejecting.count(),
        2,
        "after the cooldown one probe is allowed"
    );
}

#[tokio::test]
async fn repeated_failures_open_the_circuit_and_a_good_probe_closes_it() {
    let healthy = Arc::new(AtomicUsize::new(0));
    let flag = healthy.clone();
    let flaky = Mock::start(move |_| {
        if flag.load(Ordering::SeqCst) == 1 {
            Reply::json(200, chat_ok("recovered"))
        } else {
            Reply::json(500, err("boom"))
        }
    })
    .await;
    let other = Mock::start(|_| Reply::json(200, chat_ok("other"))).await;
    let top = "circuit:\n  failure_threshold: 3\n  cooldown_secs: 30\n  min_samples: 100\n  window: 100\n";
    let (g, clock) = gw_clock(&config(
        &[("a", &flaky.url()), ("b", &other.url())],
        "",
        top,
    ))
    .await;
    for _ in 0..3 {
        assert_eq!(
            g.post("/v1/messages", &request())
                .await
                .header("x-airmate-upstream"),
            Some("b")
        );
    }
    assert_eq!(flaky.count(), 3);
    let snap = g.shared.router.target("a/m").breaker.snapshot(clock.now());
    assert_eq!((snap.state, snap.consecutive_failures), ("open", 3));
    // While open, the target is skipped.
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.header("x-airmate-skipped"), Some("a/m:circuit-open"));
    assert_eq!(flaky.count(), 3);
    // After the cooldown a probe goes through; it fails and re-opens the circuit for longer.
    clock.advance(Duration::from_secs(31));
    g.post("/v1/messages", &request()).await;
    assert_eq!(flaky.count(), 4);
    assert_eq!(
        g.shared
            .router
            .target("a/m")
            .breaker
            .snapshot(clock.now())
            .state,
        "open"
    );
    clock.advance(Duration::from_secs(40));
    g.post("/v1/messages", &request()).await;
    assert_eq!(
        flaky.count(),
        4,
        "the cooldown doubled to 60 s, so 40 s later it is still open"
    );
    // Once the target recovers, the next probe closes the circuit.
    healthy.store(1, Ordering::SeqCst);
    clock.advance(Duration::from_secs(30));
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(r.header("x-airmate-upstream"), Some("a"), "{:?}", r.headers);
    assert_eq!(
        g.shared
            .router
            .target("a/m")
            .breaker
            .snapshot(clock.now())
            .state,
        "closed"
    );
}

#[tokio::test]
async fn a_stream_that_fails_before_its_first_byte_fails_over() {
    let silent = Mock::start(|_| Reply::sse(vec![Part::Hang])).await;
    let good = Mock::start(|_| {
        Reply::sse(vec![
            Part::Bytes(b"data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n".to_vec()),
            Part::Bytes(b"data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_vec()),
            Part::Bytes(b"data: [DONE]\n\n".to_vec()),
        ])
    })
    .await;
    let g = gw(&config(
        &[("a", &silent.url()), ("b", &good.url())],
        "    timeouts: { connect_secs: 2, first_byte_secs: 1, idle_secs: 5 }\n",
        "",
    ))
    .await;
    let r = g.post("/v1/messages", &stream_request()).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.text().contains("message_stop"));
    assert_eq!(r.header("x-airmate-attempts"), Some("2"));
    assert_eq!(r.header("x-airmate-upstream"), Some("b"));
}

#[tokio::test]
async fn there_is_no_failover_after_the_first_byte_of_a_stream_has_been_sent() {
    let breaks = Mock::start(|_| {
        Reply::sse(vec![
            Part::Bytes(b"data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"half\"},\"finish_reason\":null}]}\n\n".to_vec()),
            Part::Delay(Duration::from_millis(300)),
            Part::Reset,
        ])
    })
    .await;
    let hits = Arc::new(AtomicUsize::new(0));
    let other = Mock::start(counting(200, chat_ok("other"), &hits)).await;
    let g = gw(&config(
        &[("a", &breaks.url()), ("b", &other.url())],
        "",
        "",
    ))
    .await;
    let r = g.post("/v1/messages", &stream_request()).await;
    assert_eq!(r.status, 200);
    let text = r.text();
    assert!(text.contains("half"), "{text}");
    assert!(
        text.contains("event: error"),
        "the stream ends with an error event: {text}"
    );
    assert!(!text.contains("message_stop"));
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "a second model is never spliced into the answer"
    );
    assert_eq!(r.header("x-airmate-attempts"), Some("1"));
}

#[tokio::test]
async fn a_stream_that_breaks_after_its_first_byte_counts_against_the_target() {
    let breaks = Mock::start(|_| {
        Reply::sse(vec![
            Part::Bytes(b"data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"half\"},\"finish_reason\":null}]}\n\n".to_vec()),
            Part::Delay(Duration::from_millis(200)),
            Part::Reset,
        ])
    })
    .await;
    let top = "circuit:\n  failure_threshold: 1\n  cooldown_secs: 30\n  min_samples: 100\n  window: 100\n";
    let (g, clock) = gw_clock(&config(&[("a", &breaks.url())], "", top)).await;
    let r = g.post("/v1/messages", &stream_request()).await;
    assert!(r.text().contains("event: error"));
    for _ in 0..50 {
        if g.shared
            .router
            .target("a/m")
            .breaker
            .snapshot(clock.now())
            .state
            == "open"
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert_eq!(
        g.shared
            .router
            .target("a/m")
            .breaker
            .snapshot(clock.now())
            .state,
        "open"
    );
}

#[tokio::test]
async fn when_every_target_is_skipped_the_answer_is_529_and_lists_why() {
    let a = Mock::start(|_| Reply::json(401, err("bad key"))).await;
    let (g, _) = gw_clock(&config(&[("a", &a.url())], "", "")).await;
    let first = g.post("/v1/messages", &request()).await;
    assert_eq!(
        first.status, 403,
        "the first attempt reports the upstream's rejection"
    );
    let second = g.post("/v1/messages", &request()).await;
    assert_eq!(second.status, 529, "{}", second.text());
    assert_eq!(second.json()["error"]["type"], "overloaded_error");
    assert!(second.json()["error"]["message"]
        .as_str()
        .unwrap()
        .contains("a/m:circuit-open"));
    assert_eq!(second.header("x-airmate-skipped"), Some("a/m:circuit-open"));
    assert_eq!(a.count(), 1);
}

#[tokio::test]
async fn the_ordered_targets_follow_the_strategy_and_round_robin_alternates() {
    let a_hits = Arc::new(AtomicUsize::new(0));
    let b_hits = Arc::new(AtomicUsize::new(0));
    let a = Mock::start(counting(200, chat_ok("a"), &a_hits)).await;
    let b = Mock::start(counting(200, chat_ok("b"), &b_hits)).await;
    let mut y = config(&[("a", &a.url()), ("b", &b.url())], "", "");
    y = y.replace(
        "    name: claude\n",
        "    name: claude\n    strategy: round_robin\n",
    );
    let g = gw(&y).await;
    for _ in 0..4 {
        assert_eq!(g.post("/v1/messages", &request()).await.status, 200);
    }
    assert_eq!(
        (a_hits.load(Ordering::SeqCst), b_hits.load(Ordering::SeqCst)),
        (2, 2)
    );
}

#[tokio::test]
async fn a_conversation_sticks_to_the_target_that_served_its_previous_turn() {
    let a_hits = Arc::new(AtomicUsize::new(0));
    let flaky = Arc::new(AtomicUsize::new(0));
    let flag = flaky.clone();
    let seen = a_hits.clone();
    let a = Mock::start(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        if flag.load(Ordering::SeqCst) == 0 {
            Reply::json(503, err("busy"))
        } else {
            Reply::json(200, chat_ok("a"))
        }
    })
    .await;
    let b_hits = Arc::new(AtomicUsize::new(0));
    let b = Mock::start(counting(200, chat_ok("b"), &b_hits)).await;
    let top = "server:\n  affinity: true\ncircuit:\n  failure_threshold: 100\n  min_samples: 100\n  window: 100\n";
    let g = gw(&config(&[("a", &a.url()), ("b", &b.url())], "", top)).await;
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(
        r.header("x-airmate-upstream"),
        Some("b"),
        "a failed, so b served the first turn"
    );
    flaky.store(1, Ordering::SeqCst);
    let before = a_hits.load(Ordering::SeqCst);
    let r = g.post("/v1/messages", &request()).await;
    assert_eq!(
        r.header("x-airmate-upstream"),
        Some("b"),
        "the same conversation stays on b, which keeps its cache warm"
    );
    assert_eq!(
        a_hits.load(Ordering::SeqCst),
        before,
        "a was not even tried"
    );
    let other_conversation = json!({"model": "claude-x", "max_tokens": 16, "messages": [{"role": "user", "content": "something else entirely"}]});
    let r = g.post("/v1/messages", &other_conversation).await;
    assert_eq!(
        r.header("x-airmate-upstream"),
        Some("a"),
        "a new conversation starts from the top of the list"
    );
}

#[tokio::test]
async fn rules_route_by_allow_listed_header_and_say_which_rule_matched() {
    let a = Mock::start(|_| Reply::json(200, chat_ok("a"))).await;
    let b = Mock::start(|_| Reply::json(200, chat_ok("b"))).await;
    let yaml = format!(
        "server:\n  rule_headers: [x-team]\nupstreams:\n  - {{ name: a, url: \"{}\" }}\n  - {{ name: b, url: \"{}\" }}\nmodels:\n  - {{ match: \"claude-*\", name: main, targets: [ {{ upstream: a, model: m }} ] }}\n  - {{ match: \"claude-x\", name: special, targets: [ {{ upstream: b, model: m }} ] }}\nrules:\n  - name: data-team\n    when: {{ header: {{ x-team: \"data-*\" }} }}\n    use: special\n",
        a.url(),
        b.url()
    );
    let g = gw(&yaml).await;
    let plain = g.post("/v1/messages", &json!({"model": "claude-haiku", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]})).await;
    assert_eq!(
        (
            plain.header("x-airmate-upstream"),
            plain.header("x-airmate-rule")
        ),
        (Some("a"), None)
    );
    let routed = g.post_with("/v1/messages", &json!({"model": "claude-haiku", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}]}), &[("x-team", "data-eng")]).await;
    assert_eq!(
        (
            routed.header("x-airmate-upstream"),
            routed.header("x-airmate-rule")
        ),
        (Some("b"), Some("data-team"))
    );
}

#[tokio::test]
async fn a_rule_may_only_match_headers_on_the_allow_list() {
    let yaml = "upstreams:\n  - { name: a, url: \"http://x\" }\nmodels:\n  - { match: \"*\", targets: [ { upstream: a, model: m } ] }\nrules:\n  - { name: r, when: { header: { x-secret: \"*\" } }, use: \"*\" }\n";
    let err = airmate::config::load_str(yaml, None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("rule_headers"), "{err}");
}

#[tokio::test]
async fn targets_that_cannot_serve_the_request_are_skipped_and_the_header_says_so() {
    let a = Mock::start(|_| Reply::json(200, chat_ok("a"))).await;
    let b = Mock::start(|_| Reply::json(200, chat_ok("b"))).await;
    let top = "catalog:\n  - { match: m, vision: true }\n";
    // Target a's model name differs, so only it is described as unable to read images.
    let mut y = config(&[("a", &a.url()), ("b", &b.url())], "", top);
    y = y.replacen(
        "{ upstream: a, model: m }",
        "{ upstream: a, model: text-only }",
        1,
    );
    y.push_str("  - { match: text-only, vision: false }\n");
    let g = gw(&y).await;
    let req = json!({"model": "claude-x", "max_tokens": 5, "messages": [{"role": "user", "content": [{"type": "image", "source": {"type": "url", "url": "https://x/y.png"}}]}]});
    let r = g.post("/v1/messages", &req).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.header("x-airmate-upstream"), Some("b"));
    assert_eq!(r.header("x-airmate-skipped"), Some("a/text-only:vision"));
    assert_eq!(a.count(), 0);
}

#[tokio::test]
async fn a_request_too_big_for_every_window_is_prompt_is_too_long_without_calling_anyone() {
    let a = Mock::start(|_| Reply::json(200, chat_ok("a"))).await;
    let g = gw(&config(
        &[("a", &a.url())],
        "",
        "catalog:\n  - { match: m, context_window: 100 }\n",
    ))
    .await;
    let big = json!({"model": "claude-x", "max_tokens": 16, "messages": [{"role": "user", "content": "word ".repeat(400)}]});
    let r = g.post("/v1/messages", &big).await;
    assert_eq!(r.status, 400, "{}", r.text());
    assert!(r.json()["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("prompt is too long"));
    assert_eq!(a.count(), 0);
}

#[tokio::test]
async fn a_per_upstream_concurrency_limit_sends_the_overflow_to_the_next_target() {
    let slow = Mock::start(|_| Reply::Slow(Duration::from_millis(900))).await;
    let fast = Mock::start(|_| Reply::json(200, chat_ok("fast"))).await;
    let mut y = config(&[("a", &slow.url()), ("b", &fast.url())], "", "");
    y = y.replacen(
        "    api_key: { env: UP_KEY }\n",
        "    api_key: { env: UP_KEY }\n    max_concurrent: 1\n",
        1,
    );
    let g = Arc::new(gw(&y).await);
    let first = {
        let g = g.clone();
        tokio::spawn(async move { g.post("/v1/messages", &request()).await })
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    let second = g.post("/v1/messages", &request()).await;
    assert_eq!(second.status, 200, "{}", second.text());
    assert_eq!(second.header("x-airmate-upstream"), Some("b"));
    assert_eq!(second.header("x-airmate-skipped"), Some("a/m:concurrency"));
    let _ = first.await.unwrap();
}

#[tokio::test]
async fn attempts_and_skips_are_logged() {
    let bad = Mock::start(|_| Reply::json(503, err("busy"))).await;
    let good = Mock::start(|_| Reply::json(200, chat_ok("ok"))).await;
    let cap = Arc::new(Capture::default());
    let g = TestGateway::start_with_sink(
        &config(&[("a", &bad.url()), ("b", &good.url())], "", ""),
        &env_with_key("UP_KEY", "k"),
        cap.clone(),
    )
    .await;
    g.post("/v1/messages", &request()).await;
    let recs = cap.wait_for(3).await;
    let attempts: Vec<_> = recs
        .iter()
        .filter(|r| r.kind == airmate::obs::RecordKind::Attempt)
        .collect();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        (attempts[0].target.as_deref(), attempts[0].status),
        (Some("a/m"), 503)
    );
    assert_eq!(
        (attempts[1].target.as_deref(), attempts[1].status),
        (Some("b/m"), 200)
    );
    let req = recs
        .iter()
        .find(|r| r.kind == airmate::obs::RecordKind::Request)
        .unwrap();
    assert_eq!(
        (req.attempts, req.status, req.target.as_deref()),
        (2, 200, Some("b/m"))
    );
}
