//! Active health checks.
//!
//! An upstream with `health_check: { path, interval_secs }` is probed with a GET on that path.
//! A probe that fails (a connection error, a timeout, or a non-2xx answer) marks the upstream
//! unhealthy: its targets are skipped, and a weighted choice does not lead with it. The next good
//! probe marks it healthy again. This is most useful for local servers that come and go.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::header::AUTHORIZATION;
use hyper::{Method, Request};
use tokio::sync::watch;
use tokio::time::Instant;

use airmate_core::ir::Protocol;

use super::table::UpstreamRt;
use crate::server::Shared;

/// How long a probe may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Probes one upstream once. `None` means healthy; `Some(reason)` means it is not.
pub async fn probe(up: &UpstreamRt, path: &str) -> Option<String> {
    let uri = format!("{}{}", up.base_url, path);
    let mut b = Request::builder().method(Method::GET).uri(uri);
    if let Some(k) = up.api_key.as_ref() {
        if up.protocol == Protocol::Anthropic {
            if let Ok(v) = hyper::header::HeaderValue::from_str(k.expose()) {
                b = b.header("x-api-key", v);
            }
        } else if let Ok(v) =
            hyper::header::HeaderValue::from_str(&format!("Bearer {}", k.expose()))
        {
            b = b.header(AUTHORIZATION, v);
        }
    }
    let Ok(req) = b.body(Full::new(Bytes::new())) else {
        return Some("invalid health check URL".to_owned());
    };
    match tokio::time::timeout(PROBE_TIMEOUT, up.client.request(req)).await {
        Err(_) => Some(format!("no answer within {}s", PROBE_TIMEOUT.as_secs())),
        Ok(Err(e)) => Some(format!("cannot connect: {e}")),
        Ok(Ok(resp)) if resp.status().is_success() => None,
        Ok(Ok(resp)) => Some(format!("HTTP {}", resp.status().as_u16())),
    }
}

/// Runs the probes until `shutdown` is set. The set of upstreams is read from the current tables
/// on every tick, so a reload that adds or changes a health check takes effect without a restart.
pub async fn run(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let mut due: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = shutdown.changed() => return,
        }
        let tables = shared.tables();
        let now = Instant::now();
        for up in tables.models.upstreams() {
            let Some((path, interval)) = up.health_check.clone() else {
                continue;
            };
            if due.get(&up.name).is_some_and(|d| now < *d) {
                continue;
            }
            due.insert(up.name.clone(), now + interval);
            let up = up.clone();
            let health = shared.router.upstream_health(&up.name);
            tokio::spawn(async move {
                let result = probe(&up, &path).await;
                if let (None, true) = (&result, health.is_unhealthy()) {
                    eprintln!("airmate: upstream {} is healthy again", up.name);
                } else if let (Some(why), false) = (&result, health.is_unhealthy()) {
                    eprintln!(
                        "airmate: upstream {} failed its health check: {why}",
                        up.name
                    );
                }
                health.set(result);
            });
        }
    }
}
