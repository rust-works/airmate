//! The generation endpoints, for every inbound protocol: `/v1/messages`, `/v1/chat/completions`
//! and `/v1/responses`.
//!
//! The flow is the same whatever the client speaks: read the body, peek at what routing needs,
//! plan a route (rules, model resolution, capability filtering, strategy, affinity), then try the
//! targets in order. An *attempt* is one call to one target. A retryable failure retries the same
//! target (with backoff) or moves to the next; a non-retryable one is returned at once, since a
//! bad request does not become good by trying another provider. Once the first byte of a stream
//! has been sent there is no failover: the answer is a single upstream's.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::http::request::Parts;
use hyper::Response;
use tokio::sync::OwnedSemaphorePermit;

use airmate_core::adapters;
use airmate_core::error::{ErrorKind, GatewayError, SizeInfo, UpstreamLabel};
use airmate_core::facts::Facts;
use airmate_core::inbound::gemini as gemini_inbound;
use airmate_core::inbound::InboundCtx;
use airmate_core::ir::{IrRequest, Protocol};
use airmate_core::outbound::anthropic::PassthroughStream;
use airmate_core::outbound::embeddings;
use airmate_core::outbound::json_passthrough::{self, JsonPassthroughStream};
use airmate_core::outbound::{anthropic as passthrough, ParseCtx, UpstreamError, UpstreamRequest};
use airmate_core::policy::{ignored_header, Ignored, Parsed, PolicyConfig};
use airmate_core::stream::anthropic_render::RenderCtx;

use super::stream::{Pipe, StreamBody};
use super::upstream::{retry_after, send};
use super::{acquire, error_response, full, read_body, GwBody, Route};
use crate::router::catalog::{Skip, SkipReason};
use crate::router::engine::{exhausted, plan, PlanInput, Planned};
use crate::router::glob::glob_match;
use crate::router::RouteTarget;

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Tokens charged to a client's limiter before the call. They are settled to the real usage when
/// the answer arrives; if the request ends any other way (an error, a refusal, the client going
/// away) the charge is given back, so a failed request does not eat into the client's allowance.
pub(super) struct Reservation {
    limiter: std::sync::Arc<crate::router::limits::Limiter>,
    estimated: u64,
    settled: std::sync::atomic::AtomicBool,
}

impl Reservation {
    /// Settles to the usage the upstream reported. Only the first call counts.
    pub(super) fn settle(&self, actual: u64) {
        if !self.settled.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.limiter.settle(self.estimated, actual);
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.settle(0);
    }
}

/// Refuses a request for a model the client may not use.
pub(super) fn check_model_access(
    route: &Route,
    model: &str,
    entry_name: &str,
) -> Result<(), GatewayError> {
    let Some(c) = route.tables.clients.get(&route.identity.client) else {
        return Ok(());
    };
    if c.models.is_empty()
        || c.models
            .iter()
            .any(|g| glob_match(g, model) || glob_match(g, entry_name))
    {
        return Ok(());
    }
    Err(GatewayError::new(
        ErrorKind::Permission,
        format!("client `{}` may not use the model `{model}`", c.name),
    ))
}

/// Charges the client's rate limits for a request estimated at `est_tokens`, or refuses it with
/// `429`. The returned reservation settles the charge.
pub(super) fn reserve_client_rate(
    route: &Route,
    est_tokens: u64,
) -> Result<Option<std::sync::Arc<Reservation>>, GatewayError> {
    let Some(c) = route.tables.clients.get(&route.identity.client) else {
        return Ok(None);
    };
    let Some(rate) = c.rate else {
        return Ok(None);
    };
    let limiter = route.shared.limits.client(&c.name, rate);
    match limiter.admit(est_tokens, route.shared.router.clock.now()) {
        Ok(()) => Ok(Some(std::sync::Arc::new(Reservation {
            limiter,
            estimated: est_tokens,
            settled: std::sync::atomic::AtomicBool::new(false),
        }))),
        Err(d) => Err(GatewayError::new(
            ErrorKind::RateLimit,
            format!(
                "client `{}` exceeded its limit of {}",
                c.name,
                d.which.describe()
            ),
        )
        .with_retry_after(Some(d.retry_after.as_secs() + 1))),
    }
}

/// An error response, with the routing headers when there are any.
pub(super) fn fail_with(
    route: &Route,
    mut e: GatewayError,
    extra: &[(&'static str, String)],
) -> Response<GwBody> {
    e.message = route.tables.redact(&e.message);
    if let Some(field) = &e.policy_field {
        route.rec.rejected(field);
    }
    let mut resp = error_response(route.inbound, &e.with_request_id(route.rid.clone()));
    for (k, v) in extra {
        if let Ok(v) = hyper::header::HeaderValue::from_str(v) {
            resp.headers_mut().insert(*k, v);
        }
    }
    resp
}

pub(super) fn fail(route: &Route, e: GatewayError) -> Response<GwBody> {
    fail_with(route, e, &[])
}

pub(super) fn json_ok(route: &Route, body: Vec<u8>, extra: &[(&str, String)]) -> Response<GwBody> {
    route.log_body("response", 200, &body);
    let mut b = Response::builder()
        .status(200)
        .header("content-type", "application/json")
        .header("request-id", &route.rid);
    for (k, v) in extra {
        b = b.header(*k, v);
    }
    b.body(full(body)).expect("a valid response")
}

fn message_id() -> String {
    let mut raw = [0u8; 12];
    let _ = getrandom::fill(&mut raw);
    format!(
        "msg_{}",
        raw.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// Resolves the model name to an entry with at least one target (used by `count_tokens`).
pub(super) fn resolve(
    route: &Route,
    model: &str,
) -> Result<std::sync::Arc<crate::router::ModelEntry>, GatewayError> {
    match route.tables.models.resolve(model, route.inbound) {
        crate::router::Resolution::Entry(e) | crate::router::Resolution::Direct(e)
            if !e.targets.is_empty() =>
        {
            Ok(e)
        }
        _ => Err(GatewayError::new(
            ErrorKind::NotFound,
            format!("the model `{model}` is not mapped to an upstream model; see `airmate models`"),
        )),
    }
}

/// How a request is sent on.
enum Mode {
    /// Parsed into the IR and translated.
    Translate,
    /// Forwarded as sent (Anthropic).
    PassAnthropic,
    /// Forwarded as sent (Chat or Responses); the flag is whether the client wanted stream usage.
    PassJson { client_wants_usage: bool },
    /// An embeddings request: the model mapped, the reply naming the client's model.
    Embeddings,
}

struct Prepared {
    request: UpstreamRequest,
    mode: Mode,
    ignored: Vec<Ignored>,
}

/// The inbound request, parsed at most once however many targets are tried.
struct Source<'a> {
    body: &'a Bytes,
    facts: &'a Facts,
    parsed: OnceLock<Result<Parsed<IrRequest>, GatewayError>>,
}

impl Source<'_> {
    fn ir(&self, route: &Route) -> Result<&Parsed<IrRequest>, GatewayError> {
        let config = &route.tables.config;
        self.parsed
            .get_or_init(|| {
                let ctx = InboundCtx {
                    policy: PolicyConfig {
                        unknown_fields: config.server.unknown_fields.into(),
                    },
                    signing_key: Some(&route.tables.signing_key),
                    count_tokens: false,
                };
                match &route.gemini {
                    Some(g) => adapters::parse_gemini_request(
                        self.body,
                        &ctx,
                        &gemini_inbound::Call {
                            model: &g.model,
                            stream: g.streams(),
                        },
                    ),
                    None => adapters::parse_request(route.inbound, self.body, &ctx),
                }
                .map_err(|e| GatewayError::from_policy(&e))
            })
            .as_ref()
            .map_err(Clone::clone)
    }
}

fn prepare(
    route: &Route,
    src: &Source<'_>,
    target: &RouteTarget,
    effective_cap: Option<u32>,
) -> Result<Prepared, GatewayError> {
    let config = &route.tables.config;
    let mut core = target.core_target(config.server.think_tags);
    core.max_output_tokens_cap = match (core.max_output_tokens_cap, effective_cap) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let policy = |e: airmate_core::policy::PolicyError| GatewayError::from_policy(&e);
    if route.endpoint == super::Endpoint::Embeddings {
        return Ok(Prepared {
            request: embeddings::build(src.body, &core).map_err(policy)?,
            mode: Mode::Embeddings,
            ignored: Vec::new(),
        });
    }
    // Gemini to Gemini goes through the IR like any other pair: its bodies carry no model and the
    // path differs, so there is nothing to forward verbatim.
    if core.protocol == route.inbound && route.inbound != Protocol::Gemini {
        return match route.inbound {
            Protocol::Anthropic => {
                let request = passthrough::build_passthrough(src.body, &core).map_err(policy)?;
                let ignored = request.ignored.clone();
                Ok(Prepared {
                    request,
                    mode: Mode::PassAnthropic,
                    ignored,
                })
            }
            p => {
                let (request, wants) =
                    json_passthrough::build(src.body, &core, p).map_err(policy)?;
                let ignored = request.ignored.clone();
                Ok(Prepared {
                    request,
                    mode: Mode::PassJson {
                        client_wants_usage: wants,
                    },
                    ignored,
                })
            }
        };
    }
    let parsed = src.ir(route)?;
    let request = adapters::build_request(&parsed.value, &core).map_err(policy)?;
    let mut ignored = parsed.ignored.clone();
    for i in &request.ignored {
        if !ignored.iter().any(|x| x.name == i.name) {
            ignored.push(i.clone());
        }
    }
    Ok(Prepared {
        request,
        mode: Mode::Translate,
        ignored,
    })
}

pub(super) fn label(t: &RouteTarget) -> UpstreamLabel<'_> {
    UpstreamLabel {
        name: &t.upstream.name,
        key_setting: &t.upstream.key_setting,
    }
}

fn size_of(f: &Facts, window: Option<u64>) -> SizeInfo {
    SizeInfo {
        context_window: window,
        est_input_tokens: f.est_input_tokens,
        max_tokens: f.max_tokens,
    }
}

/// Reads a whole reply, giving up if the upstream goes quiet for `idle` between frames. The
/// first-byte timeout covers only the headers, so without this a body that stalls would hold a
/// concurrency permit for ever.
async fn collect_idle(
    mut body: Incoming,
    max: usize,
    idle: Duration,
) -> Result<Bytes, GatewayError> {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match tokio::time::timeout(idle, body.frame()).await {
            Err(_) => {
                return Err(GatewayError::new(
                    ErrorKind::Timeout,
                    format!(
                        "the upstream stopped sending its reply for {} seconds",
                        idle.as_secs()
                    ),
                ))
            }
            Ok(None) => return Ok(Bytes::from(buf)),
            Ok(Some(Err(e))) => {
                return Err(GatewayError::new(
                    ErrorKind::BadGateway,
                    format!("the upstream connection broke: {e}"),
                ))
            }
            Ok(Some(Ok(frame))) => {
                if let Ok(data) = frame.into_data() {
                    if buf.len() + data.len() > max {
                        return Err(GatewayError::new(
                            ErrorKind::BadGateway,
                            "the upstream reply was too large",
                        ));
                    }
                    buf.extend_from_slice(&data);
                }
            }
        }
    }
}

pub(super) async fn collect(body: Incoming, max: usize) -> Result<Bytes, GatewayError> {
    match http_body_util::Limited::new(body, max).collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(_) => Err(GatewayError::new(
            ErrorKind::Api,
            "the upstream reply was too large or could not be read",
        )),
    }
}

fn render_ctx(
    route: &Route,
    target: &RouteTarget,
    ir: Option<&IrRequest>,
    facts: &Facts,
    client_model: &str,
) -> RenderCtx {
    let core = target.core_target(route.tables.config.server.think_tags);
    RenderCtx {
        model: client_model.to_owned(),
        message_id: message_id(),
        signing_key: Some(route.tables.signing_key.clone()),
        origin: core.origin(),
        thinking_wanted: ir.is_some_and(|i| i.reasoning.wanted()),
        display: ir.map_or(airmate_core::ir::ReasoningDisplay::Summarized, |i| {
            i.reasoning.display
        }),
        est_input_tokens: facts.est_input_tokens,
        buffer_cap: usize::try_from(route.tables.config.server.buffer_cap_bytes)
            .unwrap_or(usize::MAX),
        created: unix_now(),
        include_usage: ir.is_some_and(|i| i.extensions.stream_usage),
    }
}

// ---- attempts -------------------------------------------------------------------------------

/// Why an attempt failed and what to do about it.
struct Failure {
    error: GatewayError,
    /// May the next target (or the same one again) be tried?
    retryable: bool,
    /// The upstream rejected its key: open the circuit and alert.
    auth: bool,
    /// A `Retry-After` the upstream gave, in seconds.
    retry_after: Option<u64>,
    /// The HTTP status seen, or the status the failure maps to.
    status: u16,
}

fn classify_status(status: u16) -> (bool, bool) {
    match status {
        401 | 403 => (true, true),
        408 | 429 | 500..=599 => (true, false),
        _ => (false, false),
    }
}

fn from_gateway(error: GatewayError) -> Failure {
    let status = error.status();
    Failure {
        error,
        retryable: true,
        auth: false,
        retry_after: None,
        status,
    }
}

/// Releases a half-open breaker's probe if the attempt is abandoned (the client went away).
struct ProbeGuard<'a> {
    planned: &'a Planned,
    armed: bool,
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.planned.state.breaker.release_probe();
        }
    }
}

/// The answer from a successful attempt, ready to be turned into a response.
enum Delivered {
    Complete { bytes: Bytes },
    Streaming { body: Incoming, first: Bytes },
}

async fn first_chunk(body: &mut Incoming, wait: Duration) -> Result<Bytes, GatewayError> {
    let read = async {
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        if !data.is_empty() {
                            return Ok(data);
                        }
                    }
                }
                Some(Err(e)) => return Err(format!("the upstream connection broke: {e}")),
                None => {
                    return Err("the upstream closed the stream without sending anything".to_owned())
                }
            }
        }
    };
    match tokio::time::timeout(wait, read).await {
        Ok(Ok(b)) => Ok(b),
        Ok(Err(detail)) => Err(GatewayError::new(ErrorKind::BadGateway, detail)),
        Err(_) => Err(GatewayError::new(
            ErrorKind::Timeout,
            "the upstream sent no data in time",
        )),
    }
}

/// One call to one target, up to the point where a stream's first byte (or a whole reply) has
/// arrived.
async fn attempt(
    route: &Route,
    parts: &Parts,
    planned: &Planned,
    prepared: &Prepared,
    facts: &Facts,
) -> Result<(Delivered, Duration), Failure> {
    let target = &planned.target;
    let started = Instant::now();
    let resp = match send(&target.upstream, &prepared.request, &parts.headers).await {
        Ok(r) => r,
        Err(e) => {
            let mut error = e.into_gateway(&target.upstream.name);
            error.message = route.tables.redact(&error.message);
            route.rec.attempt(target, error.status(), None);
            return Err(from_gateway(error));
        }
    };
    let status = resp.status.as_u16();
    if !resp.status.is_success() {
        let retry = retry_after(&resp.headers);
        let body = collect(resp.body, 1 << 20).await.unwrap_or_default();
        route.rec.attempt(
            target,
            status,
            u64::try_from(started.elapsed().as_millis()).ok(),
        );
        let e = UpstreamError::from_response(status, &body);
        let (retryable, auth) = classify_status(status);
        // What the upstream said may echo what it was sent, so it is scrubbed before it can reach
        // the breaker's last error, the status page or the client.
        let mut error = GatewayError::from_upstream(
            &e,
            label(target),
            size_of(facts, planned.caps.context_window),
            retry,
        );
        error.message = route.tables.redact(&error.message);
        return Err(Failure {
            error,
            retryable,
            auth,
            retry_after: retry,
            status,
        });
    }
    let mut body = resp.body;
    if prepared.request.stream {
        let wait = Duration::from_secs(target.upstream.timeouts.first_byte_secs);
        return match first_chunk(&mut body, wait).await {
            Ok(first) => {
                let ttfb = started.elapsed();
                route
                    .rec
                    .attempt(target, status, u64::try_from(ttfb.as_millis()).ok());
                Ok((Delivered::Streaming { body, first }, ttfb))
            }
            Err(e) => {
                route.rec.attempt(target, e.status(), None);
                Err(from_gateway(e))
            }
        };
    }
    let idle = Duration::from_secs(target.upstream.timeouts.idle_secs);
    match collect_idle(body, 256 << 20, idle).await {
        Ok(bytes) => {
            let ttfb = started.elapsed();
            route
                .rec
                .attempt(target, status, u64::try_from(ttfb.as_millis()).ok());
            Ok((Delivered::Complete { bytes }, ttfb))
        }
        Err(e) => {
            route.rec.attempt(target, e.status(), None);
            Err(from_gateway(e))
        }
    }
}

/// Exponential backoff with full jitter: a random wait up to `min(cap, base * 2^n)`.
fn backoff(n: u32, base_ms: u64, cap_ms: u64, unit: f64) -> Duration {
    let ceiling = base_ms.saturating_mul(1u64 << n.min(20)).min(cap_ms);
    Duration::from_millis((ceiling as f64 * unit) as u64)
}

fn skipped_header(skipped: &[Skip]) -> Option<String> {
    (!skipped.is_empty()).then(|| {
        skipped
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(",")
    })
}

fn routing_headers(
    route: &crate::router::engine::Route,
    target: Option<&RouteTarget>,
    attempts: u32,
    skipped: &[Skip],
    ignored: &[Ignored],
) -> Vec<(&'static str, String)> {
    let mut h = vec![("x-airmate-attempts", attempts.to_string())];
    if let Some(t) = target {
        h.push(("x-airmate-upstream", t.upstream.name.clone()));
        h.push(("x-airmate-upstream-model", t.model.clone()));
    }
    if let Some(v) = skipped_header(skipped) {
        h.push(("x-airmate-skipped", v));
    }
    if let Some(r) = &route.rule {
        h.push(("x-airmate-rule", r.clone()));
    }
    if let Some(v) = ignored_header(ignored) {
        h.push(("x-airmate-ignored", v));
    }
    h
}

/// `POST /v1/messages`, `/v1/chat/completions` and `/v1/responses`.
pub async fn generate(route: Route, parts: Parts, body: Incoming) -> Response<GwBody> {
    let config = route.tables.config.clone();
    let body = match read_body(body, &parts.headers, config.server.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => return fail(&route, e),
    };
    let peeked = match (route.endpoint, &route.gemini) {
        (super::Endpoint::Embeddings, _) => embeddings::peek(&body),
        (super::Endpoint::Generate, Some(g)) => adapters::peek_gemini(
            &body,
            &gemini_inbound::Call {
                model: &g.model,
                stream: g.streams(),
            },
        ),
        (super::Endpoint::Generate, None) => adapters::peek(route.inbound, &body),
    };
    let facts = match peeked {
        Ok(f) => f,
        Err(e) => return fail(&route, GatewayError::from_policy(&e)),
    };
    route.log_body("request", 0, &body);
    route.rec.model_asked(&facts.model);
    route.rec.stream(facts.stream);

    // Plan the route: rules, resolution, capability filtering, strategy, affinity.
    let allowed: Vec<String> = config
        .server
        .rule_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    let header = |name: &str| -> Option<String> {
        let name = name.to_ascii_lowercase();
        allowed
            .contains(&name)
            .then(|| {
                parts
                    .headers
                    .get(&name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            })
            .flatten()
    };
    // Budgets: an exhausted one is a 429, unless the client names an entry to fall back to.
    let client_cfg = route.tables.clients.get(&route.identity.client);
    let mut fallback: Option<&str> = None;
    if let Some(c) = client_cfg {
        let now = route.shared.router.clock.unix_secs();
        if let Some(ex) = c
            .budget
            .as_ref()
            .and_then(|b| route.shared.budgets.exhausted(&c.name, b, now))
        {
            match c.budget_fallback.as_deref() {
                Some(entry) => fallback = Some(entry),
                None => {
                    return fail(
                        &route,
                        GatewayError::new(
                            ErrorKind::RateLimit,
                            format!(
                                "budget exhausted: client `{}` has spent ${:.2} of its {} budget of ${:.2}; it renews at {}",
                                c.name,
                                ex.spent,
                                ex.period.name(),
                                ex.cap,
                                crate::obs::rfc3339(
                                    std::time::UNIX_EPOCH + Duration::from_secs(ex.renews_at)
                                ),
                            ),
                        )
                        .with_retry_after(Some(ex.renews_at.saturating_sub(now).max(1))),
                    );
                }
            }
        }
    }
    let planned = match plan(
        &route.tables.models,
        &route.tables.rules,
        &route.tables.catalog,
        &route.shared.router,
        &config,
        &PlanInput {
            facts: &facts,
            client: &route.identity.client,
            body: &body,
            header: &header,
            force_entry: fallback,
            embeddings: route.endpoint == super::Endpoint::Embeddings,
        },
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return fail(&route, e),
    };
    route.rec.rule(planned.rule.as_deref());
    let mut skipped = planned.skipped.clone();

    // Per-client controls: which models it may use, and how fast. A budget fallback was chosen by
    // the operator, so the client's model list does not apply to it.
    if fallback.is_none() {
        if let Err(e) = check_model_access(&route, &facts.model, &planned.entry_name) {
            return fail(&route, e);
        }
    }
    let client_limiter = match reserve_client_rate(&route, facts.est_input_tokens) {
        Ok(r) => r,
        Err(e) => return fail(&route, e),
    };

    let queue = config.server.queue_timeout_secs.map(Duration::from_secs);
    let permit = match acquire(&route.shared, queue).await {
        Ok(p) => p,
        Err(e) => return fail(&route, e),
    };

    let state = &route.shared.router;
    let failover = config.failover;
    let started = Instant::now();
    let budget = Duration::from_secs(failover.total_secs);
    let source = Source {
        body: &body,
        facts: &facts,
        parsed: OnceLock::new(),
    };
    let mut attempts: u32 = 0;
    let mut last: Option<Failure> = None;
    let mut ignored_so_far: Vec<Ignored> = Vec::new();
    let mut failed_from: Option<String> = None;

    'targets: for planned_target in &planned.targets {
        let target = &planned_target.target;
        let label = target.label();
        // State that changes while requests are in flight is checked now, not at planning time.
        if planned_target.health.is_unhealthy() {
            skipped.push(Skip {
                target: label,
                reason: SkipReason::Unhealthy,
            });
            continue;
        }
        if planned_target.state.is_cooling(state.clock.now()) {
            skipped.push(Skip {
                target: label,
                reason: SkipReason::CoolingDown,
            });
            continue;
        }
        let admit = planned_target.state.breaker.try_acquire(state.clock.now());
        if !admit.allowed() {
            skipped.push(Skip {
                target: label,
                reason: SkipReason::CircuitOpen,
            });
            continue;
        }
        let mut guard = ProbeGuard {
            planned: planned_target,
            armed: matches!(admit, crate::router::breaker::Admit::Probe),
        };
        if let Some(rate) = target.upstream.rate {
            let limiter = route.shared.limits.upstream(&target.upstream.name, rate);
            if limiter
                .admit(facts.est_input_tokens, state.clock.now())
                .is_err()
            {
                skipped.push(Skip {
                    target: label,
                    reason: SkipReason::RateLimit,
                });
                continue;
            }
        }
        let slot: Option<OwnedSemaphorePermit> = match &target.upstream.slots {
            Some(s) => match s.clone().try_acquire_owned() {
                Ok(p) => Some(p),
                Err(_) => {
                    skipped.push(Skip {
                        target: label,
                        reason: SkipReason::Concurrency,
                    });
                    continue;
                }
            },
            None => None,
        };
        let effective_cap = planned_target.caps.max_output_tokens;
        let prepared = match prepare(&route, &source, target, effective_cap) {
            Ok(p) => p,
            // A request this target cannot take, or one that cannot be parsed, is the client's to fix.
            Err(e) => {
                guard.armed = false;
                planned_target.state.breaker.release_probe();
                route
                    .rec
                    .skipped(&skipped.iter().map(ToString::to_string).collect::<Vec<_>>());
                return fail_with(&route, e, &[("x-airmate-attempts", attempts.to_string())]);
            }
        };
        for ig in &prepared.ignored {
            if !ignored_so_far.iter().any(|x| x.name == ig.name) {
                ignored_so_far.push(ig.clone());
            }
        }
        route.rec.ignored(&ignored_so_far);

        let mut retry = 0;
        loop {
            if attempts >= failover.max_attempts || started.elapsed() >= budget {
                break 'targets;
            }
            attempts += 1;
            if let Some(from) = failed_from.take() {
                if from != target.label() {
                    route.shared.metrics.inc(
                        "airmate_failovers_total",
                        &[("from", &from), ("to", &target.label())],
                    );
                }
            }
            planned_target
                .state
                .inflight
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let outcome = attempt(&route, &parts, planned_target, &prepared, &facts).await;
            planned_target
                .state
                .inflight
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            match outcome {
                Ok((delivered, ttfb)) => {
                    guard.armed = false;
                    planned_target.state.breaker.on_success();
                    planned_target.state.record_ttfb(ttfb);
                    if let Some(key) = &planned.affinity_key {
                        state.affinity_put(key, &target.label());
                    }
                    route.rec.first_byte();
                    route
                        .rec
                        .skipped(&skipped.iter().map(ToString::to_string).collect::<Vec<_>>());
                    let headers = routing_headers(
                        &planned,
                        Some(target),
                        attempts,
                        &skipped,
                        &prepared.ignored,
                    );
                    return finish(
                        &route,
                        delivered,
                        planned_target,
                        &prepared,
                        &source,
                        headers,
                        permit,
                        slot,
                        client_limiter,
                    )
                    .await;
                }
                Err(f) => {
                    let now = state.clock.now();
                    if !f.retryable {
                        // The upstream answered sensibly; the request is what is wrong.
                        guard.armed = false;
                        planned_target.state.breaker.on_success();
                        route
                            .rec
                            .skipped(&skipped.iter().map(ToString::to_string).collect::<Vec<_>>());
                        let headers =
                            routing_headers(&planned, Some(target), attempts, &skipped, &[]);
                        return fail_with(&route, f.error, &headers);
                    }
                    guard.armed = false;
                    failed_from = Some(target.label());
                    if f.auth {
                        // The key is wrong; hammering it is harmful. Open the circuit and tell the operator.
                        planned_target.state.breaker.trip(now, &f.error.message);
                        state
                            .auth_alerts
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        eprintln!(
                            "airmate: alert: {} rejected its API key (HTTP {}); check {}",
                            target.upstream.name, f.status, target.upstream.key_setting
                        );
                    } else {
                        planned_target
                            .state
                            .breaker
                            .on_failure(now, &f.error.message);
                    }
                    let wait_for = f.retry_after;
                    last = Some(f);
                    if last.as_ref().is_some_and(|f| f.auth) {
                        continue 'targets;
                    }
                    // A pause longer than the router will wait: mark the target and fail over.
                    if let Some(secs) = wait_for {
                        if secs > failover.retry_after_ceiling_secs {
                            // An upstream controls `secs`: clamp it so the sum cannot overflow.
                            planned_target.state.cool_until(
                                now.saturating_add(Duration::from_secs(secs.min(86_400))),
                            );
                            continue 'targets;
                        }
                    }
                    if retry < failover.retries {
                        let mut wait = backoff(
                            retry,
                            failover.backoff_base_ms,
                            failover.backoff_cap_ms,
                            state.random.unit(),
                        );
                        if let Some(secs) = wait_for {
                            wait = wait.max(Duration::from_secs(secs));
                        }
                        retry += 1;
                        if started.elapsed() + wait >= budget {
                            break 'targets;
                        }
                        tokio::time::sleep(wait).await;
                        // The breaker may have opened while we waited, or have handed us its probe.
                        let readmit = planned_target.state.breaker.try_acquire(state.clock.now());
                        if !readmit.allowed() {
                            skipped.push(Skip {
                                target: target.label(),
                                reason: SkipReason::CircuitOpen,
                            });
                            continue 'targets;
                        }
                        guard.armed = matches!(readmit, crate::router::breaker::Admit::Probe);
                        continue;
                    }
                    continue 'targets;
                }
            }
        }
    }

    route
        .rec
        .skipped(&skipped.iter().map(ToString::to_string).collect::<Vec<_>>());
    let headers = routing_headers(&planned, None, attempts, &skipped, &[]);
    match last {
        Some(f) => fail_with(&route, f.error, &headers),
        None => fail_with(&route, exhausted(&planned.entry_name, &skipped), &headers),
    }
}

/// Prices a finished request, then writes its ledger line and counts it against the budget.
#[derive(Clone)]
struct Meter {
    shared: std::sync::Arc<super::Shared>,
    rec: super::record::Recorder,
    request_id: String,
    client: String,
    model_asked: String,
    target: String,
    upstream_model: String,
    price: Option<crate::config::schema::Price>,
}

impl Meter {
    fn new(route: &Route, planned: &Planned, model_asked: &str) -> Self {
        let target = &planned.target;
        Self {
            shared: route.shared.clone(),
            rec: route.rec.clone(),
            request_id: route.rid.clone(),
            client: route.identity.client.clone(),
            model_asked: model_asked.to_owned(),
            target: target.label(),
            upstream_model: target.model.clone(),
            // The target's own price wins, then the catalog's, then the price table's.
            price: planned
                .caps
                .price
                .or_else(|| route.tables.prices.lookup(&target.model)),
        }
    }

    /// Records usage. A request with no price has cost `None`, which is not zero and adds
    /// nothing to a budget.
    fn settle(&self, usage: &airmate_core::ir::Usage) {
        let cost = self.price.map(|p| crate::usage::cost_usd(&p, usage));
        self.rec.cost(cost);
        let empty = usage.input_tokens
            + usage.output_tokens
            + usage.cache_read_input_tokens
            + usage.reasoning_tokens
            == 0;
        if empty {
            return;
        }
        let unix = self.shared.router.clock.unix_secs();
        if let Some(c) = cost {
            self.shared.budgets.record(&self.client, unix, c);
        }
        self.shared.usage.record(&crate::usage::UsageRecord {
            ts: crate::obs::rfc3339(std::time::UNIX_EPOCH + Duration::from_secs(unix)),
            unix,
            request_id: self.request_id.clone(),
            client: self.client.clone(),
            model_asked: self.model_asked.clone(),
            target: self.target.clone(),
            upstream_model: self.upstream_model.clone(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_tokens: usage.cache_read_input_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cost_usd: cost,
        });
    }
}

/// Turns a delivered answer into the response for the client.
#[allow(clippy::too_many_arguments)]
async fn finish(
    route: &Route,
    delivered: Delivered,
    planned: &Planned,
    prepared: &Prepared,
    source: &Source<'_>,
    headers: Vec<(&'static str, String)>,
    permit: OwnedSemaphorePermit,
    slot: Option<OwnedSemaphorePermit>,
    client_limiter: Option<std::sync::Arc<Reservation>>,
) -> Response<GwBody> {
    let config = &route.tables.config;
    let target = &planned.target;
    let meter = Meter::new(route, planned, &source.facts.model);
    let settle = |usage: &airmate_core::ir::Usage| {
        meter.settle(usage);
        if let Some(l) = &client_limiter {
            l.settle(usage.input_tokens + usage.cache_read_input_tokens);
        }
    };
    let client_model = source.facts.model.clone();
    let ir = match prepared.mode {
        Mode::Translate => source.ir(route).ok().map(|p| &p.value),
        _ => None,
    };
    match delivered {
        Delivered::Complete { bytes } => {
            let out = match &prepared.mode {
                Mode::Translate => {
                    let core = target.core_target(config.server.think_tags);
                    let ctx = ParseCtx::new(&core, &prepared.request);
                    match adapters::parse_response(core.protocol, 200, &bytes, &ctx) {
                        Ok(c) => {
                            route.rec.usage(&c.usage);
                            settle(&c.usage);
                            let rctx = render_ctx(route, target, ir, source.facts, &client_model);
                            adapters::render_response_for(route.inbound, &c, &rctx)
                        }
                        Err(e) => {
                            let size = size_of(source.facts, planned.caps.context_window);
                            return fail_with(
                                route,
                                GatewayError::from_upstream(&e, label(target), size, None),
                                &headers,
                            );
                        }
                    }
                }
                Mode::PassAnthropic => {
                    let (rewritten, summary) = passthrough::rewrite_response(&bytes, &client_model);
                    route.rec.usage(&summary.usage);
                    settle(&summary.usage);
                    rewritten
                }
                Mode::PassJson { .. } => {
                    let (rewritten, usage) =
                        json_passthrough::rewrite_response(&bytes, &client_model, route.inbound);
                    route.rec.usage(&usage);
                    settle(&usage);
                    rewritten
                }
                Mode::Embeddings => {
                    let (rewritten, usage) = embeddings::rewrite_response(&bytes, &client_model);
                    route.rec.usage(&usage);
                    settle(&usage);
                    rewritten
                }
            };
            drop((permit, slot));
            json_ok(
                route,
                out,
                &headers
                    .iter()
                    .map(|(k, v)| (*k, v.clone()))
                    .collect::<Vec<_>>(),
            )
        }
        Delivered::Streaming { body, first } => {
            let core = target.core_target(config.server.think_tags);
            let pipe = match &prepared.mode {
                Mode::Translate => {
                    let parser = adapters::stream_parser(
                        core.protocol,
                        ParseCtx::new(&core, &prepared.request),
                    );
                    let render = adapters::stream_renderer(
                        route.inbound,
                        render_ctx(route, target, ir, source.facts, &client_model),
                    );
                    Pipe::translate(parser, render)
                }
                Mode::PassAnthropic => Pipe::Passthrough(PassthroughStream::new(&client_model)),
                // An embeddings request never streams, so no stream of this mode is delivered.
                Mode::Embeddings => Pipe::Json(JsonPassthroughStream::new(
                    route.inbound,
                    &client_model,
                    false,
                )),
                Mode::PassJson { client_wants_usage } => Pipe::Json(JsonPassthroughStream::new(
                    route.inbound,
                    &client_model,
                    *client_wants_usage,
                )),
            };
            route.rec.defer();
            let rec = route.rec.clone();
            let state = planned.state.clone();
            let clock = route.shared.router.clock.clone();
            let limiter = client_limiter.clone();
            let meter = meter.clone();
            let body_cap = route
                .shared
                .bodies
                .enabled()
                .then(|| route.shared.bodies.max_bytes());
            let shared = route.shared.clone();
            let (rid, client) = (route.rid.clone(), route.identity.client.clone());
            let mut stream = StreamBody::new(
                body,
                &first,
                pipe,
                Duration::from_secs(target.upstream.timeouts.idle_secs),
                Duration::from_secs(config.server.keepalive_secs),
                vec![permit].into_iter().chain(slot).collect(),
                Some(Box::new(move |summary| {
                    // A stream that breaks after its first byte counts against the target.
                    if summary.error.is_some() && !summary.cancelled {
                        state.breaker.on_failure(
                            clock.now(),
                            summary.error.as_deref().unwrap_or("stream error"),
                        );
                    }
                    if let Some(l) = &limiter {
                        l.settle(
                            summary.usage.input_tokens + summary.usage.cache_read_input_tokens,
                        );
                    }
                    meter.settle(&summary.usage);
                    if body_cap.is_some() {
                        let cap = shared.bodies.max_bytes();
                        let mut r = crate::bodylog::make_record(
                            &rid,
                            &client,
                            "response",
                            200,
                            &summary.captured,
                            cap,
                        );
                        r.bytes = summary.sent_bytes;
                        r.truncated = summary.sent_bytes > summary.captured.len();
                        shared.bodies.record(&r);
                    }
                    rec.finish_stream(&summary);
                })),
            );
            if let Some(cap) = body_cap {
                stream = stream.with_tap(cap);
            }
            stream = stream.with_stop(route.shared.hard_stop.subscribe());
            let tables = route.tables.clone();
            stream = stream.with_scrub(Box::new(move |b| tables.redact_bytes(b)));
            let mut b = Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("cache-control", "no-cache")
                .header("request-id", &route.rid);
            for (k, v) in headers {
                b = b.header(k, v);
            }
            b.body(stream.boxed_unsync()).expect("a valid response")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_classify_into_retry_and_alert() {
        assert_eq!(classify_status(401), (true, true));
        assert_eq!(classify_status(403), (true, true));
        for s in [408, 429, 500, 502, 503, 504, 529] {
            assert_eq!(classify_status(s), (true, false), "{s}");
        }
        for s in [400, 404, 413, 422, 418] {
            assert_eq!(classify_status(s), (false, false), "{s}");
        }
    }

    #[test]
    fn backoff_is_exponential_with_full_jitter_and_capped() {
        assert_eq!(backoff(0, 250, 5000, 1.0), Duration::from_millis(250));
        assert_eq!(backoff(1, 250, 5000, 1.0), Duration::from_millis(500));
        assert_eq!(backoff(3, 250, 5000, 1.0), Duration::from_millis(2000));
        assert_eq!(
            backoff(10, 250, 5000, 1.0),
            Duration::from_millis(5000),
            "capped"
        );
        assert_eq!(
            backoff(2, 250, 5000, 0.0),
            Duration::ZERO,
            "full jitter reaches zero"
        );
        assert_eq!(backoff(2, 250, 5000, 0.5), Duration::from_millis(500));
        assert!(
            backoff(63, u64::MAX, u64::MAX, 1.0) > Duration::ZERO,
            "no overflow"
        );
    }

    #[test]
    fn the_skipped_header_joins_skips() {
        assert_eq!(skipped_header(&[]), None);
        let s = [
            Skip {
                target: "a/m".into(),
                reason: SkipReason::Vision,
            },
            Skip {
                target: "b/m".into(),
                reason: SkipReason::CircuitOpen,
            },
        ];
        assert_eq!(
            skipped_header(&s).as_deref(),
            Some("a/m:vision,b/m:circuit-open")
        );
    }
}
