//! The Anthropic Messages endpoints: `/v1/messages`, `/v1/messages/count_tokens`, `/v1/models`.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::http::request::Parts;
use hyper::Response;

use airmate_core::error::{ErrorKind, GatewayError, SizeInfo, UpstreamLabel};
use airmate_core::estimate::{estimate_input_tokens, ESTIMATE_HEADER_VALUE};
use airmate_core::facts::Facts;
use airmate_core::inbound::{anthropic as inbound, InboundCtx};
use airmate_core::ir::{IrRequest, Protocol};
use airmate_core::outbound::anthropic::PassthroughStream;
use airmate_core::outbound::chat_parse::ChatStream;
use airmate_core::outbound::responses_parse::ResponsesStream;
use airmate_core::outbound::{
    anthropic as passthrough, chat, chat_parse, responses, responses_parse, ParseCtx,
    UpstreamError, UpstreamRequest,
};
use airmate_core::policy::{ignored_header, Ignored, PolicyConfig};
use airmate_core::stream::anthropic_render::{render_message, AnthropicRender, RenderCtx};
use airmate_core::stream::StreamParse;

use super::stream::{Pipe, StreamBody};
use super::upstream::{retry_after, send, UpstreamResponse};
use super::{acquire, error_response, full, read_body, GwBody, Route};
use crate::router::{ModelEntry, Resolution, RouteTarget};

const IN: Protocol = Protocol::Anthropic;

fn fail(route: &Route, e: GatewayError) -> Response<GwBody> {
    error_response(IN, &e.with_request_id(route.rid.clone()))
}

fn json_ok(route: &Route, body: Vec<u8>, extra: &[(&str, String)]) -> Response<GwBody> {
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

/// Resolves the model to an entry and picks its first target.
fn resolve<'a>(
    route: &'a Route,
    model: &str,
) -> Result<(Arc<ModelEntry>, &'a Route), GatewayError> {
    match route.tables.models.resolve(model, IN) {
        Resolution::Entry(e) | Resolution::Direct(e) if !e.targets.is_empty() => Ok((e, route)),
        _ => Err(GatewayError::new(
            ErrorKind::NotFound,
            format!("the model `{model}` is not mapped to an upstream model; see `airmate models`"),
        )),
    }
}

/// Everything about a request once it has been prepared for its target.
struct Prepared {
    request: UpstreamRequest,
    ir: Option<IrRequest>,
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
    if target.upstream.protocol == Protocol::Anthropic {
        let request = passthrough::build_passthrough(body, &core)
            .map_err(|e| GatewayError::from_policy(&e))?;
        let ignored = request.ignored.clone();
        return Ok(Prepared {
            request,
            ir: None,
            facts,
            ignored,
        });
    }
    let ctx = InboundCtx {
        policy: PolicyConfig {
            unknown_fields: config.server.unknown_fields.into(),
        },
        signing_key: Some(&route.tables.signing_key),
        count_tokens: false,
    };
    let parsed = inbound::parse_request(body, &ctx).map_err(|e| GatewayError::from_policy(&e))?;
    let ir = parsed.value;
    let request = match core.protocol {
        Protocol::Chat => chat::build(&ir, &core),
        Protocol::Responses => responses::build(&ir, &core),
        other => {
            return Err(GatewayError::new(
                ErrorKind::InvalidRequest,
                format!(
                    "upstream protocol `{}` is not supported for this request",
                    other.as_str()
                ),
            ))
        }
    }
    .map_err(|e| GatewayError::from_policy(&e))?;
    let mut ignored = parsed.ignored;
    for i in &request.ignored {
        if !ignored.iter().any(|x| x.name == i.name) {
            ignored.push(i.clone());
        }
    }
    let facts = Facts::from_ir(&ir);
    Ok(Prepared {
        request,
        ir: Some(ir),
        facts,
        ignored,
    })
}

fn label<'a>(t: &'a RouteTarget) -> UpstreamLabel<'a> {
    UpstreamLabel {
        name: &t.upstream.name,
        key_setting: &t.upstream.key_setting,
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
    GatewayError::from_upstream(
        &e,
        label(target),
        SizeInfo {
            context_window: None,
            est_input_tokens: p.facts.est_input_tokens,
            max_tokens: p.facts.max_tokens,
        },
        retry,
    )
}

async fn collect(body: Incoming, max: usize) -> Result<Bytes, GatewayError> {
    match http_body_util::Limited::new(body, max).collect().await {
        Ok(c) => Ok(c.to_bytes()),
        Err(_) => Err(GatewayError::new(
            ErrorKind::Api,
            "the upstream reply was too large or could not be read",
        )),
    }
}

/// `POST /v1/messages`.
pub async fn messages(route: Route, parts: Parts, body: Incoming) -> Response<GwBody> {
    let config = route.tables.config.clone();
    let body = match read_body(body, &parts.headers, config.server.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => return fail(&route, e),
    };
    let facts = match passthrough::peek(&body) {
        Ok(f) => f,
        Err(e) => return fail(&route, GatewayError::from_policy(&e)),
    };
    let (entry, _) = match resolve(&route, &facts.model) {
        Ok(r) => r,
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
    let ttfb = u64::try_from(sent_at.elapsed().as_millis()).ok();
    route.rec.attempt(&target, resp.status.as_u16(), ttfb);
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
    let out = if let Some(ir) = &prepared.ir {
        let core = target.core_target(config.server.think_tags);
        let ctx = ParseCtx::new(&core, &prepared.request);
        let completion = match core.protocol {
            Protocol::Responses => responses_parse::parse_response(200, &bytes, &ctx),
            _ => chat_parse::parse_response(200, &bytes, &ctx),
        };
        match completion {
            Ok(c) => {
                route.rec.usage(&c.usage);
                let rctx = RenderCtx {
                    model: client_model,
                    message_id: message_id(),
                    signing_key: Some(route.tables.signing_key.clone()),
                    origin: core.origin(),
                    thinking_wanted: ir.reasoning.wanted(),
                    display: ir.reasoning.display,
                    est_input_tokens: prepared.facts.est_input_tokens,
                    buffer_cap: usize::try_from(config.server.buffer_cap_bytes)
                        .unwrap_or(usize::MAX),
                    created: unix_now(),
                    include_usage: false,
                };
                render_message(&c, &rctx)
            }
            Err(e) => {
                let size = SizeInfo {
                    context_window: None,
                    est_input_tokens: prepared.facts.est_input_tokens,
                    max_tokens: prepared.facts.max_tokens,
                };
                return fail(
                    &route,
                    GatewayError::from_upstream(&e, label(&target), size, None),
                );
            }
        }
    } else {
        let (rewritten, summary) = passthrough::rewrite_response(&bytes, &client_model);
        route.rec.usage(&summary.usage);
        rewritten
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
    let pipe = if let Some(ir) = &prepared.ir {
        let ctx = ParseCtx::new(&core, &prepared.request);
        let parser: Box<dyn StreamParse> = match core.protocol {
            Protocol::Responses => Box::new(ResponsesStream::new(ctx)),
            _ => Box::new(ChatStream::new(ctx)),
        };
        let render = AnthropicRender::new(RenderCtx {
            model: client_model,
            message_id: message_id(),
            signing_key: Some(route.tables.signing_key.clone()),
            origin: core.origin(),
            thinking_wanted: ir.reasoning.wanted(),
            display: ir.reasoning.display,
            est_input_tokens: prepared.facts.est_input_tokens,
            buffer_cap: usize::try_from(config.server.buffer_cap_bytes).unwrap_or(usize::MAX),
            created: unix_now(),
            include_usage: false,
        });
        Pipe::translate(parser, Box::new(render))
    } else {
        Pipe::Passthrough(PassthroughStream::new(&client_model))
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

fn reply_headers(target: &RouteTarget, ignored: &[Ignored]) -> Vec<(&'static str, String)> {
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

/// `POST /v1/messages/count_tokens`: an estimate, unless the upstream can count.
pub async fn count_tokens(route: Route, parts: Parts, body: Incoming) -> Response<GwBody> {
    let config = route.tables.config.clone();
    let body = match read_body(body, &parts.headers, config.server.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => return fail(&route, e),
    };
    let facts = match passthrough::peek(&body) {
        Ok(f) => f,
        Err(e) => return fail(&route, GatewayError::from_policy(&e)),
    };
    let (entry, _) = match resolve(&route, &facts.model) {
        Ok(r) => r,
        Err(e) => return fail(&route, e),
    };
    let target = entry.targets[0].clone();
    let estimate = |n: u64| -> Response<GwBody> {
        json_ok(
            &route,
            serde_json::to_vec(&serde_json::json!({ "input_tokens": n })).expect("serialises"),
            &[("x-airmate-token-estimate", ESTIMATE_HEADER_VALUE.to_owned())],
        )
    };

    if target.upstream.protocol == Protocol::Anthropic {
        let core = target.core_target(config.server.think_tags);
        let request = match passthrough::build_count_tokens(&body, &core) {
            Ok(r) => r,
            Err(e) => return fail(&route, GatewayError::from_policy(&e)),
        };
        let resp = match send(&target.upstream, &request, &parts.headers).await {
            Ok(r) => r,
            Err(e) => return fail(&route, e.into_gateway(&target.upstream.name)),
        };
        let status = resp.status;
        if status == hyper::StatusCode::NOT_FOUND {
            return estimate(facts.est_input_tokens);
        }
        if !status.is_success() {
            let p = Prepared {
                request,
                ir: None,
                facts,
                ignored: vec![],
            };
            return fail(&route, error_from_upstream(resp, &target, &p).await);
        }
        return match collect(resp.body, 1 << 20).await {
            Ok(b) => json_ok(&route, b.to_vec(), &[]),
            Err(e) => fail(&route, e),
        };
    }

    let ctx = InboundCtx {
        policy: PolicyConfig {
            unknown_fields: config.server.unknown_fields.into(),
        },
        signing_key: Some(&route.tables.signing_key),
        count_tokens: true,
    };
    match inbound::parse_request(&body, &ctx) {
        Ok(p) => estimate(estimate_input_tokens(&p.value)),
        Err(e) => fail(&route, GatewayError::from_policy(&e)),
    }
}

fn model_json(name: &str) -> serde_json::Value {
    serde_json::json!({"type": "model", "id": name, "display_name": name, "created_at": "1970-01-01T00:00:00Z"})
}

/// The names `/v1/models` lists: model entries that are exact names, not globs.
fn listed(route: &Route) -> Vec<String> {
    route
        .tables
        .models
        .entries()
        .iter()
        .filter(|e| !crate::router::glob::is_glob(&e.pattern))
        .map(|e| e.pattern.clone())
        .collect()
}

/// `GET /v1/models`.
pub fn models(route: &Route, _parts: &Parts) -> Response<GwBody> {
    let names = listed(route);
    let data: Vec<_> = names.iter().map(|n| model_json(n)).collect();
    let body = serde_json::json!({
        "data": data,
        "has_more": false,
        "first_id": names.first(),
        "last_id": names.last(),
    });
    json_ok(route, serde_json::to_vec(&body).expect("serialises"), &[])
}

/// `GET /v1/models/{id}`.
pub fn model(route: &Route, _parts: &Parts, id: &str) -> Response<GwBody> {
    if listed(route).iter().any(|n| n == id) {
        json_ok(
            route,
            serde_json::to_vec(&model_json(id)).expect("serialises"),
            &[],
        )
    } else {
        fail(
            route,
            GatewayError::new(
                ErrorKind::NotFound,
                format!("the model `{id}` does not exist"),
            ),
        )
    }
}

/// Seconds since the Unix epoch.
#[must_use]
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
