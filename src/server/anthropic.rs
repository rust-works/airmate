//! The Anthropic-specific endpoints: `/v1/messages/count_tokens` and the model listings.

use hyper::body::Incoming;
use hyper::http::request::Parts;
use hyper::Response;

use airmate_core::adapters;
use airmate_core::error::{ErrorKind, GatewayError};
use airmate_core::estimate::{estimate_input_tokens, ESTIMATE_HEADER_VALUE};
use airmate_core::facts::Facts;
use airmate_core::inbound::{anthropic as inbound, InboundCtx};
use airmate_core::ir::Protocol;
use airmate_core::outbound::anthropic as passthrough;
use airmate_core::policy::PolicyConfig;

use super::generate::{
    check_model_access, collect, fail, json_ok, label, reserve_client_rate, resolve,
};
use super::upstream::{retry_after, send};
use super::{acquire, read_body, GwBody, Route};

/// `POST /v1/messages/count_tokens`: an estimate, unless the upstream can count.
pub async fn count_tokens(route: Route, parts: Parts, body: Incoming) -> Response<GwBody> {
    let config = route.tables.config.clone();
    let body = match read_body(body, &parts.headers, config.server.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => return fail(&route, e),
    };
    let facts: Facts = match adapters::peek(Protocol::Anthropic, &body) {
        Ok(f) => f,
        Err(e) => return fail(&route, GatewayError::from_policy(&e)),
    };
    let entry = match resolve(&route, &facts.model) {
        Ok(e) => e,
        Err(e) => return fail(&route, e),
    };
    // The same client controls apply as to a generation: the model it may use, and its rate.
    if let Err(e) = check_model_access(&route, &facts.model, &entry.name) {
        return fail(&route, e);
    }
    let _rate = match reserve_client_rate(&route, 0) {
        Ok(r) => r,
        Err(e) => return fail(&route, e),
    };
    let _permit = match acquire(
        &route.shared,
        config
            .server
            .queue_timeout_secs
            .map(std::time::Duration::from_secs),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => return fail(&route, e),
    };
    // The first target that is not known to be down; if all are, the estimate stands in.
    let clock = route.shared.router.clock.now();
    let usable = entry.targets.iter().find(|t| {
        let state = route.shared.router.target(&t.label());
        state.breaker.snapshot(clock).state != "open"
            && !route
                .shared
                .router
                .upstream_health(&t.upstream.name)
                .is_unhealthy()
    });
    let estimate = |n: u64| -> Response<GwBody> {
        json_ok(
            &route,
            serde_json::to_vec(&serde_json::json!({ "input_tokens": n })).expect("serialises"),
            &[("x-airmate-token-estimate", ESTIMATE_HEADER_VALUE.to_owned())],
        )
    };

    if let Some(target) = usable.filter(|t| t.upstream.protocol == Protocol::Anthropic) {
        let target = target.clone();
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
            let retry = retry_after(&resp.headers);
            let bytes = collect(resp.body, 1 << 20).await.unwrap_or_default();
            let e = airmate_core::outbound::UpstreamError::from_response(status.as_u16(), &bytes);
            let size = airmate_core::error::SizeInfo {
                context_window: None,
                est_input_tokens: facts.est_input_tokens,
                max_tokens: facts.max_tokens,
            };
            return fail(
                &route,
                GatewayError::from_upstream(&e, label(&target), size, retry),
            );
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

/// Whether the client wants the Anthropic shape: it sent `anthropic-version`, or `x-api-key`
/// without a bearer header. OpenAI clients send `Authorization: Bearer`.
fn anthropic_shape(parts: &Parts) -> bool {
    parts.headers.contains_key("anthropic-version")
        || (parts.headers.contains_key("x-api-key") && !parts.headers.contains_key("authorization"))
}

fn model_json(name: &str, anthropic: bool) -> serde_json::Value {
    if anthropic {
        serde_json::json!({"type": "model", "id": name, "display_name": name, "created_at": "1970-01-01T00:00:00Z"})
    } else {
        serde_json::json!({"id": name, "object": "model", "created": 0, "owned_by": "airmate"})
    }
}

/// `GET /v1/models`, in whichever shape the client expects.
pub fn models(route: &Route, parts: &Parts) -> Response<GwBody> {
    let anthropic = anthropic_shape(parts);
    let names = listed(route);
    let data: Vec<_> = names.iter().map(|n| model_json(n, anthropic)).collect();
    let body = if anthropic {
        serde_json::json!({"data": data, "has_more": false, "first_id": names.first(), "last_id": names.last()})
    } else {
        serde_json::json!({"object": "list", "data": data})
    };
    json_ok(route, serde_json::to_vec(&body).expect("serialises"), &[])
}

/// `GET /v1/models/{id}`.
pub fn model(route: &Route, parts: &Parts, id: &str) -> Response<GwBody> {
    if listed(route).iter().any(|n| n == id) {
        json_ok(
            route,
            serde_json::to_vec(&model_json(id, anthropic_shape(parts))).expect("serialises"),
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
