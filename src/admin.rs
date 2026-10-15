//! The admin interface: a small HTTP server on a Unix-domain socket in the runtime directory
//! (`0600`, in a `0700` directory), not on the data-plane port, so a data-plane token cannot
//! administer the gateway.
//!
//! `GET /status`, `GET /metrics` (Prometheus text), `GET /usage` (live spend against budgets) and
//! `POST /reload`. On platforms without Unix sockets the admin endpoint is not available.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::server::reload::ReloadReport;
use crate::server::Shared;
use crate::usage::{budget::Period, Budgets};

/// What a reload request does: re-read the configuration and swap it in.
pub type ReloadFn = Arc<dyn Fn() -> Result<ReloadReport, String> + Send + Sync>;

fn circuit_value(state: &str) -> f64 {
    match state {
        "closed" => 0.0,
        "half-open" => 1.0,
        _ => 2.0,
    }
}

/// The Prometheus exposition: counters and histograms, then the gauges read from live state.
#[must_use]
pub fn metrics_text(shared: &Shared) -> String {
    let now = shared.router.clock.now();
    let mut gauges: Vec<crate::metrics::Gauge> = Vec::new();
    for (label, t) in shared.router.targets_snapshot() {
        gauges.push((
            "airmate_circuit_state",
            vec![("target", label.clone())],
            circuit_value(t.breaker.snapshot(now).state),
        ));
        gauges.push((
            "airmate_inflight",
            vec![("target", label)],
            t.inflight.load(std::sync::atomic::Ordering::SeqCst) as f64,
        ));
    }
    shared.metrics.render(&gauges)
}

/// A short, stable fingerprint of the configuration in force.
#[must_use]
pub fn config_hash(shared: &Shared) -> String {
    let json = serde_json::to_string(&*shared.tables().config).unwrap_or_default();
    crate::auth::hash_key(&json)
        .trim_start_matches("sha256:")
        .chars()
        .take(12)
        .collect()
}

/// The gateway's state at a glance. `ok` is false when an upstream is failing its health check or
/// a circuit is open.
#[must_use]
pub fn status_json(shared: &Shared) -> Value {
    let tables = shared.tables();
    let now = shared.router.clock.now();
    let targets = shared.router.targets_snapshot();
    let mut ok = true;
    let mut upstreams: Vec<_> = tables.models.upstreams().collect();
    upstreams.sort_by(|a, b| a.name.cmp(&b.name));
    let upstreams: Vec<Value> = upstreams
        .into_iter()
        .map(|u| {
            let health = shared.router.upstream_health(&u.name);
            let prefix = format!("{}/", u.name);
            let ts: Vec<Value> = targets
                .iter()
                .filter(|(l, _)| l.starts_with(&prefix))
                .map(|(l, t)| {
                    let b = t.breaker.snapshot(now);
                    if b.state == "open" {
                        ok = false;
                    }
                    json!({
                        "target": l,
                        "circuit": b.state,
                        "consecutive_failures": b.consecutive_failures,
                        "last_error": b.last_error,
                        "next_probe_in_secs": b.next_probe_in.map(|d| d.as_secs()),
                        "inflight": t.inflight.load(std::sync::atomic::Ordering::SeqCst),
                        "ttfb_ms": t.ttfb_ms(1),
                        "cooling_down": t.is_cooling(now),
                    })
                })
                .collect();
            if health.is_unhealthy() {
                ok = false;
            }
            json!({
                "name": u.name,
                "url": u.base_url,
                "protocol": u.protocol.as_str(),
                "healthy": !health.is_unhealthy(),
                "health_error": health.last_error(),
                "targets": ts,
            })
        })
        .collect();
    json!({
        "ok": ok,
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "uptime_secs": shared.started.elapsed().as_secs(),
        "listen": shared.listen.to_string(),
        "config_hash": config_hash(shared),
        "requests_in_flight": tables.config.server.max_concurrent as usize - shared.permits.available_permits().min(tables.config.server.max_concurrent as usize),
        "upstream_key_rejections": shared.router.auth_alerts.load(std::sync::atomic::Ordering::SeqCst),
        "upstreams": upstreams,
        "recent_errors": shared.metrics.recent_errors(),
        "ignored_fields": shared.metrics.ignored_tally(),
    })
}

fn spend_json(
    budgets: &Budgets,
    client: &str,
    b: &crate::config::schema::Budget,
    now: u64,
) -> Value {
    let mut periods = serde_json::Map::new();
    for (p, cap) in [
        (Period::Day, b.usd_per_day),
        (Period::Week, b.usd_per_week),
        (Period::Month, b.usd_per_month),
    ] {
        if let Some(cap) = cap {
            periods.insert(
                p.name().to_owned(),
                json!({ "spent_usd": budgets.spent(client, p, now), "cap_usd": cap }),
            );
        }
    }
    Value::Object(periods)
}

/// Live spend against each client's budgets.
#[must_use]
pub fn usage_json(shared: &Shared) -> Value {
    let now = shared.router.clock.unix_secs();
    let tables = shared.tables();
    let mut clients: Vec<_> = tables
        .clients
        .values()
        .filter(|c| c.budget.is_some())
        .collect();
    clients.sort_by(|a, b| a.name.cmp(&b.name));
    let rows: serde_json::Map<String, Value> = clients
        .into_iter()
        .filter_map(|c| {
            Some((
                c.name.clone(),
                spend_json(&shared.budgets, &c.name, c.budget.as_ref()?, now),
            ))
        })
        .collect();
    json!({ "budgets": rows })
}

#[cfg(unix)]
pub use unix::{bind, call, serve};

#[cfg(unix)]
mod unix {
    use std::convert::Infallible;
    use std::path::Path;
    use std::sync::Arc;

    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use hyper::body::Incoming;
    use hyper::service::service_fn;
    use hyper::{Method, Request, Response};
    use hyper_util::rt::TokioIo;
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::watch;

    use super::{metrics_text, status_json, usage_json, ReloadFn};
    use crate::server::Shared;

    /// Binds the admin socket, owner-only. A socket left by a dead process is replaced; one that
    /// still answers belongs to a running gateway and is refused.
    ///
    /// # Errors
    ///
    /// The directory or socket cannot be created, or another gateway is serving it.
    pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
        crate::sys::bind_private_socket(path)
    }

    fn reply(status: u16, content_type: &str, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
        Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(Full::new(body.into()))
            .expect("a valid response")
    }

    fn json_reply(status: u16, v: &serde_json::Value) -> Response<Full<Bytes>> {
        reply(status, "application/json", v.to_string())
    }

    async fn route(
        shared: Arc<Shared>,
        reload: ReloadFn,
        req: Request<Incoming>,
    ) -> Response<Full<Bytes>> {
        match (req.method().clone(), req.uri().path()) {
            (Method::GET, "/status") => json_reply(200, &status_json(&shared)),
            (Method::GET, "/metrics") => reply(
                200,
                "text/plain; version=0.0.4; charset=utf-8",
                metrics_text(&shared),
            ),
            (Method::GET, "/usage") => json_reply(200, &usage_json(&shared)),
            (Method::POST, "/reload") => {
                // Reloading reads files and runs credential helpers: keep it off the async threads.
                let outcome = tokio::task::spawn_blocking(move || reload())
                    .await
                    .unwrap_or_else(|_| Err("the reload panicked".to_owned()));
                match outcome {
                    Ok(r) => json_reply(
                        200,
                        &serde_json::json!({
                            "reloaded": true,
                            "warnings": r.warnings,
                            "restart_required": r.restart_required,
                        }),
                    ),
                    Err(e) => {
                        json_reply(422, &serde_json::json!({ "reloaded": false, "error": e }))
                    }
                }
            }
            (_, p @ ("/status" | "/metrics" | "/usage" | "/reload")) => json_reply(
                405,
                &serde_json::json!({ "error": format!("wrong method for {p}") }),
            ),
            _ => json_reply(
                404,
                &serde_json::json!({ "error": "no such admin endpoint" }),
            ),
        }
    }

    /// Serves the admin interface until `shutdown` is set, then removes the socket.
    pub async fn serve(
        listener: UnixListener,
        path: std::path::PathBuf,
        shared: Arc<Shared>,
        reload: ReloadFn,
        mut shutdown: watch::Receiver<bool>,
    ) {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let (shared, reload) = (shared.clone(), reload.clone());
                    tokio::spawn(async move {
                        let service = service_fn(move |req| {
                            let (shared, reload) = (shared.clone(), reload.clone());
                            async move { Ok::<_, Infallible>(route(shared, reload, req).await) }
                        });
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
                _ = shutdown.changed() => break,
            }
        }
        let _ = std::fs::remove_file(path);
    }

    /// Sends one request to the admin socket at `path`; returns the status and body.
    ///
    /// # Errors
    ///
    /// The socket cannot be reached or the exchange fails.
    pub async fn call(path: &Path, method: &str, uri: &str) -> Result<(u16, String), String> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| format!("cannot reach the admin socket {}: {e}", path.display()))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(conn);
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "airmate")
            .body(Full::new(Bytes::new()))
            .map_err(|e| e.to_string())?;
        let resp = sender.send_request(req).await.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        Ok((status, String::from_utf8_lossy(&body).into_owned()))
    }
}
