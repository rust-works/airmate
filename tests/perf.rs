//! The performance budget (spec section 14.8): what the gateway adds to a request, what an idle
//! connection costs, and whether a long stream's allocations grow.
//!
//! Ignored by default because the numbers only mean something in a release build on a quiet
//! machine. The scheduled `perf-budget` workflow runs it:
//!
//! ```text
//! cargo test --release --test perf -- --ignored --nocapture
//! ```
//!
//! The budgets are absolute (p99 under 1 ms to translate and route a 50 KB request; under 64 KiB
//! per idle connection; flat allocation across a long stream). Set `AIRMATE_PERF_BASELINE` to a
//! JSON file from an earlier run and any metric more than 20 % worse than its baseline fails too;
//! `AIRMATE_PERF_WRITE` saves this run's metrics for the next one.

mod support;

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use airmate::config::load_str;
use airmate::router::clock::{OsRandom, SystemClock};
use airmate::router::engine::{plan, PlanInput};
use airmate::router::state::RouterState;
use airmate::secret::{Secret, SystemResolver};
use airmate::server::Tables;
use airmate_core::adapters;
use airmate_core::inbound::InboundCtx;
use airmate_core::ir::{Origin, Protocol, ReasoningDisplay};
use airmate_core::outbound::{chat_parse::ChatStream, ParseCtx, Target};
use airmate_core::signature::SigningKey;
use airmate_core::stream::anthropic_render::{AnthropicRender, RenderCtx};
use airmate_core::stream::StreamParse;
use serde_json::{json, Value};

/// Counts allocations and live bytes so the test can see growth.
struct Counting;

static LIVE: AtomicI64 = AtomicI64::new(0);
static COUNT: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call to the system allocator unchanged and only adds to counters.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as i64, Ordering::Relaxed);
        COUNT.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as i64, Ordering::Relaxed);
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LIVE.fetch_add(new_size as i64 - layout.size() as i64, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn live() -> i64 {
    LIVE.load(Ordering::Relaxed)
}

/// An Anthropic Messages request of about 50 KB: a system prompt, a long conversation with tool
/// use, and a few tools.
fn big_request() -> Vec<u8> {
    let filler = "The quick brown fox jumps over the lazy dog, again and again, to make a paragraph of realistic length. ";
    let mut messages = Vec::new();
    for i in 0..22 {
        messages.push(json!({"role": "user", "content": format!("{i}: {}", filler.repeat(10))}));
        messages.push(json!({"role": "assistant", "content": [
            {"type": "text", "text": filler.repeat(8)},
            {"type": "tool_use", "id": format!("tu_{i}"), "name": "read_file", "input": {"path": format!("/src/file_{i}.rs")}}]}));
        messages.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": format!("tu_{i}"), "content": filler.repeat(3)}]}));
    }
    messages.push(json!({"role": "user", "content": "now summarise"}));
    let tool = |n: &str| json!({"name": n, "description": "A tool that does one thing well and says so at length in its description.", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["path"]}});
    let body = json!({"model": "claude-sonnet-4-5", "max_tokens": 4096, "stream": true, "system": filler.repeat(15),
        "messages": messages, "tools": [tool("read_file"), tool("write_file"), tool("run_tests")],
        "thinking": {"type": "enabled", "budget_tokens": 4096}});
    serde_json::to_vec(&body).unwrap()
}

fn p(sorted: &[Duration], q: f64) -> Duration {
    sorted[((sorted.len() as f64 * q).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1)]
}

fn us(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "a release-mode measurement; run by the perf-budget workflow"]
async fn perf_budget() {
    let release = !cfg!(debug_assertions);
    let mut metrics: BTreeMap<&str, f64> = BTreeMap::new();

    // ---- added latency: parse, route, translate, and render a reply ------------------------
    let body = big_request();
    assert!(
        body.len() > 45_000 && body.len() < 70_000,
        "the request is {} bytes",
        body.len()
    );
    let yaml = "upstreams:\n  - { name: up, url: \"http://127.0.0.1:1\" }\nmodels:\n  - { match: \"claude-*\", targets: [ { upstream: up, model: gpt-5 } ] }\n";
    let cfg = Arc::new(load_str(yaml, None).unwrap());
    let (tables, _) = Tables::build(
        cfg.clone(),
        &SystemResolver::new(&|_| None),
        &Secret::new("t"),
    )
    .unwrap();
    let state = RouterState::new(
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom),
        (&cfg.circuit).into(),
        1024,
    );
    let none = |_: &str| None;
    let key = SigningKey::from_token("perf");
    let reply = json!({"id": "c", "model": "gpt-4.1", "choices": [{"index": 0, "message": {"role": "assistant", "content": "a short answer"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 12000, "completion_tokens": 20}}).to_string().into_bytes();
    let core_target = tables.models.entries()[0].targets[0].core_target(true);
    let render = RenderCtx {
        model: "claude-sonnet-4-5".into(),
        message_id: "msg_perf".into(),
        signing_key: Some(key.clone()),
        origin: core_target.origin(),
        thinking_wanted: true,
        display: ReasoningDisplay::Summarized,
        est_input_tokens: 0,
        buffer_cap: 1 << 20,
        created: 1,
        include_usage: false,
    };

    let mut samples = Vec::with_capacity(2_000);
    for i in 0..2_200 {
        let t = Instant::now();
        let facts = adapters::peek(Protocol::Anthropic, &body).unwrap();
        let route = plan(
            &tables.models,
            &tables.rules,
            &tables.catalog,
            &state,
            &cfg,
            &PlanInput {
                facts: &facts,
                client: "default",
                body: &body,
                header: &none,
                force_entry: None,
                embeddings: false,
            },
        )
        .await
        .unwrap();
        let parsed = adapters::parse_request(
            Protocol::Anthropic,
            &body,
            &InboundCtx {
                signing_key: Some(&key),
                ..InboundCtx::default()
            },
        )
        .unwrap();
        let upstream = adapters::build_request(
            &parsed.value,
            &Target {
                ..core_target.clone()
            },
        )
        .unwrap();
        let ctx = ParseCtx::new(&core_target, &upstream);
        let completion = adapters::parse_response(Protocol::Chat, 200, &reply, &ctx).unwrap();
        let out = adapters::render_response_for(Protocol::Anthropic, &completion, &render);
        std::hint::black_box((route, out, upstream.body.len()));
        if i >= 200 {
            samples.push(t.elapsed());
        }
    }
    samples.sort();
    let (p50, p99) = (p(&samples, 0.5), p(&samples, 0.99));
    println!(
        "added latency for a {} KB request: p50 {:.0} us, p99 {:.0} us",
        body.len() / 1024,
        us(p50),
        us(p99)
    );
    metrics.insert("added_latency_p99_us", us(p99));
    if release {
        assert!(
            p99 < Duration::from_millis(1),
            "p99 {:.0} us is over the 1 ms budget",
            us(p99)
        );
    }

    // ---- allocation growth over a long stream ---------------------------------------------
    let stream_ctx = ParseCtx {
        origin: Origin {
            upstream: "perf".into(),
            protocol: Protocol::Chat,
        },
        tool_names: Default::default(),
        think_tags: true,
    };
    let chunk = |n: usize| {
        format!(
            "data: {}\n\n",
            json!({"id": "c", "model": "m", "choices": [{"index": 0, "delta": {"content": format!("token{n} ")}, "finish_reason": null}]})
        )
    };
    let run = |chunks: usize| {
        let mut parser = ChatStream::new(stream_ctx.clone());
        let mut render = AnthropicRender::new(RenderCtx {
            thinking_wanted: false,
            ..render.clone()
        });
        let mut sent = 0usize;
        let before = (COUNT.load(Ordering::Relaxed), live());
        for n in 0..chunks {
            for ev in parser.push(chunk(n).as_bytes()) {
                sent += render.push(&ev).len();
            }
        }
        let mid = live();
        let _ = parser.finish();
        let _ = render.finish();
        let allocs = COUNT.load(Ordering::Relaxed) - before.0;
        (allocs as f64 / chunks as f64, mid - before.1, sent)
    };
    let (per_chunk_small, retained_small, _) = run(1_000);
    let (per_chunk_large, retained_large, sent) = run(20_000);
    println!("stream: {per_chunk_small:.1} allocations per chunk at 1k chunks, {per_chunk_large:.1} at 20k; retained {retained_small} then {retained_large} bytes; {sent} bytes out");
    metrics.insert("allocations_per_chunk", per_chunk_large);
    // Nothing is kept per chunk: 20x the chunks must not mean 20x the memory.
    assert!(
        retained_large - retained_small < 256 * 1024,
        "retained memory grew with the stream: {retained_small} -> {retained_large}"
    );
    assert!(
        (per_chunk_large - per_chunk_small).abs() < per_chunk_small * 0.2 + 1.0,
        "allocations per chunk drifted: {per_chunk_small} -> {per_chunk_large}"
    );

    // ---- an idle connection ---------------------------------------------------------------
    let gateway = support::TestGateway::start("upstreams:\n  - { name: up, url: \"http://127.0.0.1:1\" }\nmodels:\n  - { match: \"*\", targets: [ { upstream: up, model: m } ] }\n").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let before = live();
    let n = 300;
    let mut conns = Vec::new();
    for _ in 0..n {
        conns.push(tokio::net::TcpStream::connect(gateway.addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let per_conn = (live() - before) as f64 / f64::from(n);
    println!("an idle connection costs about {per_conn:.0} bytes of heap");
    metrics.insert("idle_connection_bytes", per_conn);
    assert!(
        per_conn < 64.0 * 1024.0,
        "{per_conn:.0} bytes per idle connection is over the 64 KiB budget"
    );
    drop(conns);

    // ---- against a baseline ---------------------------------------------------------------
    if let Some(path) = std::env::var_os("AIRMATE_PERF_BASELINE").map(std::path::PathBuf::from) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let base: BTreeMap<String, f64> = serde_json::from_str(&text).unwrap();
            for (k, v) in &metrics {
                if let Some(b) = base.get(*k) {
                    // Allow a floor of absolute noise so a tiny metric does not trip on 20 %.
                    assert!(
                        *v <= b * 1.2 + 25.0,
                        "{k} regressed more than 20 %: {b:.1} -> {v:.1}"
                    );
                }
            }
            println!("within 20 % of the baseline in {}", path.display());
        } else {
            println!("no baseline at {} yet", path.display());
        }
    }
    if let Some(path) = std::env::var_os("AIRMATE_PERF_WRITE") {
        std::fs::write(path, serde_json::to_string_pretty(&metrics).unwrap()).unwrap();
    }
    let _: Value = json!({});
}
