//! The admin interface: the Unix-socket server, `/status`, `/metrics`, `/usage` and `/reload`.
#![cfg(unix)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

use airmate::admin::{bind, call, serve, ReloadFn};
use airmate::auth::hash_key;
use airmate::server::reload::ReloadReport;
use serde_json::{json, Value};
use support::{Mock, Reply, TestGateway};
use tokio::sync::watch;

fn chat_ok() -> Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}})
}

fn socket_path(name: &str) -> PathBuf {
    PathBuf::from(format!(
        "/tmp/am-admin-{}-{name}/run/admin.sock",
        std::process::id()
    ))
}

struct Admin {
    path: PathBuf,
    stop: watch::Sender<bool>,
    done: tokio::task::JoinHandle<()>,
}

impl Admin {
    async fn start(g: &TestGateway, name: &str, reload: ReloadFn) -> Self {
        let path = socket_path(name);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
        let listener = bind(&path).unwrap();
        let (stop, rx) = watch::channel(false);
        let done = tokio::spawn(serve(listener, path.clone(), g.shared.clone(), reload, rx));
        Self { path, stop, done }
    }

    async fn get(&self, uri: &str) -> (u16, String) {
        call(&self.path, "GET", uri).await.unwrap()
    }

    async fn finish(self) {
        let _ = self.stop.send(true);
        let _ = self.done.await;
        let _ = std::fs::remove_dir_all(self.path.parent().unwrap().parent().unwrap());
    }
}

fn no_reload() -> ReloadFn {
    Arc::new(|| Err("not supported here".to_owned()))
}

fn yaml(mock: &Mock, extra: &str) -> String {
    format!(
        "server:\n  affinity: false\nupstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\nclients:\n  - name: ci\n    key_hash: \"{}\"\n    budget: {{ usd_per_day: 5.0 }}\n{extra}",
        mock.url(),
        hash_key("ci-key"),
    )
}

#[tokio::test]
async fn the_socket_is_owner_only_and_removed_on_shutdown() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let g = TestGateway::start(&yaml(&mock, "")).await;
    let a = Admin::start(&g, "perms", no_reload()).await;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&a.path), 0o600);
    assert_eq!(mode(a.path.parent().unwrap()), 0o700);
    let path = a.path.clone();
    a.finish().await;
    assert!(!path.exists());
}

#[tokio::test]
async fn a_live_socket_is_refused_and_a_stale_one_is_replaced() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let g = TestGateway::start(&yaml(&mock, "")).await;
    let a = Admin::start(&g, "stale", no_reload()).await;
    let err = bind(&a.path).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse, "{err}");
    assert!(err.to_string().contains("already serving"));
    // Simulate a crash: the file stays but nothing listens.
    let path = a.path.clone();
    a.stop.send(true).unwrap();
    a.done.await.unwrap();
    let _stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
    drop(_stale);
    assert!(path.exists(), "a dead process leaves its socket file");
    let fresh = bind(&path).expect("a stale socket is replaced");
    drop(fresh);
    let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
}

#[tokio::test]
async fn status_reports_upstreams_circuits_ignored_fields_and_recent_errors() {
    let mock = Mock::start(|r| {
        if String::from_utf8_lossy(&r.body).contains("boom") {
            Reply::json(400, json!({"error": {"message": "bad"}}))
        } else {
            Reply::json(200, chat_ok())
        }
    })
    .await;
    let g = TestGateway::start(&yaml(&mock, "")).await;
    let a = Admin::start(&g, "status", no_reload()).await;
    let ok = json!({"model": "claude-x", "max_tokens": 8, "metadata": {"user_id": "u"}, "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(g.post("/v1/messages", &ok).await.status, 200);
    let bad = json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "boom"}]});
    assert_eq!(g.post("/v1/messages", &bad).await.status, 400);
    // The request log is written as responses finish; give the last write a moment.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let (code, body) = a.get("/status").await;
    assert_eq!(code, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(v["listen"], g.addr.to_string());
    assert_eq!(v["config_hash"].as_str().unwrap().len(), 12);
    let up = &v["upstreams"][0];
    assert_eq!(
        (up["name"].as_str(), up["healthy"].as_bool()),
        (Some("up"), Some(true))
    );
    assert_eq!(up["targets"][0]["target"], "up/m");
    assert_eq!(up["targets"][0]["circuit"], "closed");
    assert_eq!(v["ignored_fields"]["metadata"], 1, "{v}");
    assert_eq!(v["recent_errors"][0]["status"], 400);
    a.finish().await;
}

#[tokio::test]
async fn an_open_circuit_makes_the_gateway_unhealthy() {
    let mock = Mock::start(|_| Reply::json(500, json!({"error": {"message": "down"}}))).await;
    let cfg = yaml(
        &mock,
        "circuit:\n  failure_threshold: 1\n  cooldown_secs: 3600\n  max_cooldown_secs: 3600\n",
    );
    let g = TestGateway::start(&cfg).await;
    let a = Admin::start(&g, "unhealthy", no_reload()).await;
    let r = g.post("/v1/messages", &json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]})).await;
    assert_eq!(r.status, 500, "{}", r.text());
    let v: Value = serde_json::from_str(&a.get("/status").await.1).unwrap();
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["upstreams"][0]["targets"][0]["circuit"], "open");
    let (_, metrics) = a.get("/metrics").await;
    assert!(
        metrics.contains("airmate_circuit_state{target=\"up/m\"} 2"),
        "{metrics}"
    );
    a.finish().await;
}

#[tokio::test]
async fn metrics_follow_the_documented_names() {
    let good = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let bad = Mock::start(|_| Reply::json(503, json!({"error": {"message": "down"}}))).await;
    let cfg = format!(
        "server:\n  affinity: false\nfailover:\n  retries: 0\nupstreams:\n  - {{ name: bad, url: \"{}\" }}\n  - {{ name: good, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: bad, model: m }}, {{ upstream: good, model: m, price: {{ input_per_mtok: 1000, output_per_mtok: 1000 }} }} ] }}\n",
        bad.url(),
        good.url()
    );
    let g = TestGateway::start(&cfg).await;
    let a = Admin::start(&g, "metrics", no_reload()).await;
    let body = json!({"model": "claude-x", "max_tokens": 8, "metadata": {"user_id": "u"}, "messages": [{"role": "user", "content": "hi"}]});
    assert_eq!(g.post("/v1/messages", &body).await.status, 200);
    let bad_req = json!({"model": "claude-x", "max_tokens": 8, "mcp_servers": [{}], "messages": [{"role": "user", "content": "hi"}]});
    let rej = g.post("/v1/messages", &bad_req).await;
    assert_eq!(rej.status, 400, "{}", rej.text());
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let (code, text) = a.get("/metrics").await;
    assert_eq!(code, 200);
    for want in [
        "airmate_requests_total{inbound=\"anthropic\",client=\"default\",target=\"good/m\",status=\"200\"} 1",
        "airmate_attempts_total{target=\"bad/m\",outcome=\"server_error\"} 1",
        "airmate_attempts_total{target=\"good/m\",outcome=\"ok\"} 1",
        "airmate_failovers_total{from=\"bad/m\",to=\"good/m\"} 1",
        "airmate_tokens_total{target=\"good/m\",kind=\"input\"} 5",
        "airmate_ignored_fields_total{field=\"metadata\"} 1",
        "airmate_policy_rejections_total{field=\"mcp_servers\"}",
        "airmate_request_duration_seconds_count{inbound=\"anthropic\"} 2",
        "airmate_ttfb_seconds_count{target=\"good/m\"} 1",
        "airmate_inflight{target=\"good/m\"} 0",
        "airmate_circuit_state{target=\"good/m\"} 0",
    ] {
        assert!(text.contains(want), "missing {want}\n{text}");
    }
    assert!(
        text.contains("airmate_cost_usd_total{client=\"default\",target=\"good/m\"}"),
        "{text}"
    );
    a.finish().await;
}

#[tokio::test]
async fn usage_shows_live_spend_against_budgets() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let cfg = yaml(&mock, "").replace(
        "m }",
        "m, price: { input_per_mtok: 1000, output_per_mtok: 2000 } }",
    );
    let g = TestGateway::start(&cfg).await;
    let a = Admin::start(&g, "usage", no_reload()).await;
    let r = g.post_with("/v1/messages", &json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]}), &[("x-api-key", "ci-key")]).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v: Value = serde_json::from_str(&a.get("/usage").await.1).unwrap();
    let day = &v["budgets"]["ci"]["daily"];
    assert_eq!(day["cap_usd"], 5.0);
    assert!(
        (day["spent_usd"].as_f64().unwrap() - 0.007).abs() < 1e-9,
        "{v}"
    );
    a.finish().await;
}

#[tokio::test]
async fn reload_runs_the_callback_and_reports_rejection_as_422() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let g = TestGateway::start(&yaml(&mock, "")).await;
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = count.clone();
    let reload: ReloadFn = Arc::new(move || {
        if c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            Ok(ReloadReport {
                warnings: vec!["w1".into()],
                restart_required: vec!["server.port".into()],
            })
        } else {
            Err("server.max_concurrent: must be at least 1".into())
        }
    });
    let a = Admin::start(&g, "reload", reload).await;
    let (code, body) = call(&a.path, "POST", "/reload").await.unwrap();
    assert_eq!(code, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (
            v["reloaded"].as_bool(),
            v["warnings"][0].as_str(),
            v["restart_required"][0].as_str()
        ),
        (Some(true), Some("w1"), Some("server.port"))
    );
    let (code, body) = call(&a.path, "POST", "/reload").await.unwrap();
    assert_eq!(code, 422);
    assert!(body.contains("max_concurrent"));
    assert_eq!(call(&a.path, "GET", "/reload").await.unwrap().0, 405);
    assert_eq!(call(&a.path, "GET", "/nope").await.unwrap().0, 404);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 2);
    a.finish().await;
}

#[tokio::test]
async fn the_data_plane_does_not_serve_admin_endpoints() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let g = TestGateway::start(&yaml(&mock, "")).await;
    for path in ["/status", "/metrics", "/reload", "/usage"] {
        let r = g.get(path).await;
        assert_eq!(
            r.status, 404,
            "{path}: a data-plane token cannot administer the gateway"
        );
    }
}
