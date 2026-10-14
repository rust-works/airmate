//! The generation endpoints, for every inbound protocol: `/v1/messages`, `/v1/chat/completions`
//! and `/v1/responses`.
//!
//! The flow is the same whatever the client speaks: read the body, peek at what routing needs,
//! resolve the model, prepare the upstream request (a passthrough when both sides speak the same
//! protocol, a translation otherwise), call the upstream, and send the answer back in the client's
//! protocol.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::http::request::Parts;
use hyper::Response;

use airmate_core::adapters;
use airmate_core::error::{ErrorKind, GatewayError, SizeInfo, UpstreamLabel};
use airmate_core::facts::Facts;
use airmate_core::inbound::InboundCtx;
use airmate_core::ir::{IrRequest, Protocol};
use airmate_core::outbound::anthropic::PassthroughStream;
use airmate_core::outbound::json_passthrough::{self, JsonPassthroughStream};
use airmate_core::outbound::{anthropic as passthrough, ParseCtx, UpstreamError, UpstreamRequest};
use airmate_core::policy::{ignored_header, Ignored, PolicyConfig};
use airmate_core::stream::anthropic_render::RenderCtx;

use super::stream::{Pipe, StreamBody};
use super::upstream::{retry_after, send, UpstreamResponse};
use super::{acquire, error_response, full, read_body, GwBody, Route};
use crate::router::{Resolution, RouteTarget};

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(super) fn fail(route: &Route, e: GatewayError) -> Response<GwBody> {
    error_response(route.inbound, &e.with_request_id(route.rid.clone()))
}

pub(super) fn json_ok(route: &Route, body: Vec<u8>, extra: &[(&str, String)]) -> Response<GwBody> {
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

/// Resolves the model name to an entry with at least one target.
pub(super) fn resolve(
    route: &Route,
    model: &str,
) -> Result<std::sync::Arc<crate::router::ModelEntry>, GatewayError> {
    match route.tables.models.resolve(model, route.inbound) {
        Resolution::Entry(e) | Resolution::Direct(e) if !e.targets.is_empty() => Ok(e),
        _ => Err(GatewayError::new(
            ErrorKind::NotFound,
            format!("the model `{model}` is not mapped to an upstream model; see `airmate models`"),
        )),
    }
}

/// How a request is sent on.
enum Mode {
    /// Parsed into the IR and translated.
    Translate(Box<IrRequest>),
    /// Forwarded as sent (Anthropic).
    PassAnthropic,
    /// Forwarded as sent (Chat or Responses); the flag is whether the client wanted stream usage.
    PassJson { client_wants_usage: bool },
}

struct Prepared {
    request: UpstreamRequest,
    mode: Mode,
    facts: Facts,
    ignored: Vec<Ignored>,
}

fn prepare(
    route: &Route,
    body: &Bytes,
    target: &RouteTarget,
    facts: Facts,
) -> Result<Prepared, GatewayError> {
    let config = &route.tables.config;
    let core = target.core_target(config.server.think_tags);
    let policy = |e: airmate_core::policy::PolicyError| GatewayError::from_policy(&e);
    if core.protocol == route.inbound {
        return match route.inbound {
            Protocol::Anthropic => {
                let request = passthrough::build_passthrough(body, &core).map_err(policy)?;
                let ignored = request.ignored.clone();
                Ok(Prepared {
                    request,
                    mode: Mode::PassAnthropic,
                    facts,
                    ignored,
                })
            }
            p => {
                let (request, wants) = json_passthrough::build(body, &core, p).map_err(policy)?;
                Ok(Prepared {
                    request,
                    mode: Mode::PassJson {
                        client_wants_usage: wants,
                    },
                    facts,
                    ignored: Vec::new(),
                })
            }
        };
    }
    let ctx = InboundCtx {
        policy: PolicyConfig {
            unknown_fields: config.server.unknown_fields.into(),
        },
        signing_key: Some(&route.tables.signing_key),
        count_tokens: false,
    };
    let parsed = adapters::parse_request(route.inbound, body, &ctx).map_err(policy)?;
    let ir = parsed.value;
    let request = adapters::build_request(&ir, &core).map_err(policy)?;
    let mut ignored = parsed.ignored;
    for i in &request.ignored {
        if !ignored.iter().any(|x| x.name == i.name) {
            ignored.push(i.clone());
        }
    }
    let facts = Facts::from_ir(&ir);
    Ok(Prepared {
        request,
        mode: Mode::Translate(Box::new(ir)),
        facts,
        ignored,
    })
}

pub(super) fn label(t: &RouteTarget) -> UpstreamLabel<'_> {
    UpstreamLabel {
        name: &t.upstream.name,
        key_setting: &t.upstream.key_setting,
    }
}

fn size_of(p: &Prepared) -> SizeInfo {
    SizeInfo {
        context_window: None,
        est_input_tokens: p.facts.est_input_tokens,
        max_tokens: p.facts.max_tokens,
    }
}

async fn error_from_upstream(
    resp: UpstreamResponse,
    target: &RouteTarget,
    p: &Prepared,
) -> GatewayError {
    let status = resp.status.as_u16();
    let retry = retry_after(&resp.headers);
    let body = collect(resp.body, 1 << 20).await.unwrap_or_default();
    let e = UpstreamError::from_response(status, &body);
    GatewayError::from_upstream(&e, label(target), size_of(p), retry)
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

/// `POST /v1/messages`, `/v1/chat/completions` and `/v1/responses`.
pub async fn generate(route: Route, parts: Parts, body: Incoming) -> Response<GwBody> {
    let config = route.tables.config.clone();
    let body = match read_body(body, &parts.headers, config.server.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => return fail(&route, e),
    };
    let facts = match adapters::peek(route.inbound, &body) {
        Ok(f) => f,
        Err(e) => return fail(&route, GatewayError::from_policy(&e)),
    };
    let entry = match resolve(&route, &facts.model) {
        Ok(e) => e,
        Err(e) => return fail(&route, e),
    };
    let target = entry.targets[0].clone();
    let client_model = facts.model.clone();
    route.rec.model_asked(&client_model);
    let prepared = match prepare(&route, &body, &target, facts) {
        Ok(p) => p,
        Err(e) => return fail(&route, e),
    };
    route.rec.ignored(&prepared.ignored);
    route.rec.stream(prepared.request.stream);
    let queue = config.server.queue_timeout_secs.map(Duration::from_secs);
    let permit = match acquire(&route.shared, queue).await {
        Ok(p) => p,
        Err(e) => return fail(&route, e),
    };
    let sent_at = std::time::Instant::now();
    let resp = match send(&target.upstream, &prepared.request, &parts.headers).await {
        Ok(r) => r,
        Err(e) => {
            let e = e.into_gateway(&target.upstream.name);
            route.rec.attempt(&target, e.status(), None);
            return fail(&route, e);
        }
    };
    route.rec.attempt(
        &target,
        resp.status.as_u16(),
        u64::try_from(sent_at.elapsed().as_millis()).ok(),
    );
    route.rec.first_byte();
    if !resp.status.is_success() {
        let e = error_from_upstream(resp, &target, &prepared).await;
        return fail(&route, e);
    }
    if prepared.request.stream {
        return stream_response(&route, resp, &target, &prepared, client_model, permit).await;
    }
    let bytes = match collect(resp.body, 256 << 20).await {
        Ok(b) => b,
        Err(e) => return fail(&route, e),
    };
    let headers = reply_headers(&target, &prepared.ignored);
    let out = match &prepared.mode {
        Mode::Translate(ir) => {
            let core = target.core_target(config.server.think_tags);
            let ctx = ParseCtx::new(&core, &prepared.request);
            match adapters::parse_response(core.protocol, 200, &bytes, &ctx) {
                Ok(c) => {
                    route.rec.usage(&c.usage);
                    let rctx =
                        render_ctx(&route, &target, Some(ir), &prepared.facts, &client_model);
                    adapters::render_response_for(route.inbound, &c, &rctx)
                }
                Err(e) => {
                    return fail(
                        &route,
                        GatewayError::from_upstream(&e, label(&target), size_of(&prepared), None),
                    )
                }
            }
        }
        Mode::PassAnthropic => {
            let (rewritten, summary) = passthrough::rewrite_response(&bytes, &client_model);
            route.rec.usage(&summary.usage);
            rewritten
        }
        Mode::PassJson { .. } => {
            let (rewritten, usage) =
                json_passthrough::rewrite_response(&bytes, &client_model, route.inbound);
            route.rec.usage(&usage);
            rewritten
        }
    };
    json_ok(&route, out, &headers)
}

/// Waits for the first upstream data frame: a failure before the first byte is still an error
/// response, not a broken stream.
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

async fn stream_response(
    route: &Route,
    resp: UpstreamResponse,
    target: &RouteTarget,
    prepared: &Prepared,
    client_model: String,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Response<GwBody> {
    let config = &route.tables.config;
    let up = &target.upstream;
    let mut body = resp.body;
    let first = match first_chunk(&mut body, Duration::from_secs(up.timeouts.first_byte_secs)).await
    {
        Ok(b) => b,
        Err(e) => return fail(route, e),
    };
    let core = target.core_target(config.server.think_tags);
    let pipe = match &prepared.mode {
        Mode::Translate(ir) => {
            let parser =
                adapters::stream_parser(core.protocol, ParseCtx::new(&core, &prepared.request));
            let render = adapters::stream_renderer(
                route.inbound,
                render_ctx(route, target, Some(ir), &prepared.facts, &client_model),
            );
            Pipe::translate(parser, render)
        }
        Mode::PassAnthropic => Pipe::Passthrough(PassthroughStream::new(&client_model)),
        Mode::PassJson { client_wants_usage } => Pipe::Json(JsonPassthroughStream::new(
            route.inbound,
            &client_model,
            *client_wants_usage,
        )),
    };
    route.rec.defer();
    let rec = route.rec.clone();
    let stream = StreamBody::new(
        body,
        &first,
        pipe,
        Duration::from_secs(up.timeouts.idle_secs),
        Duration::from_secs(config.server.keepalive_secs),
        Some(permit),
        Some(Box::new(move |summary| rec.finish_stream(&summary))),
    );
    let mut b = Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("request-id", &route.rid);
    for (k, v) in reply_headers(target, &prepared.ignored) {
        b = b.header(k, v);
    }
    b.body(stream.boxed_unsync()).expect("a valid response")
}

pub(super) fn reply_headers(
    target: &RouteTarget,
    ignored: &[Ignored],
) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("x-airmate-upstream", target.upstream.name.clone()),
        ("x-airmate-upstream-model", target.model.clone()),
        ("x-airmate-attempts", "1".to_owned()),
    ];
    if let Some(v) = ignored_header(ignored) {
        h.push(("x-airmate-ignored", v));
    }
    h
}
