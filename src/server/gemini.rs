//! The Gemini-shaped endpoints besides generation: `countTokens` and the model list.

use hyper::body::Incoming;
use hyper::http::request::Parts;
use hyper::Response;

use airmate_core::adapters;
use airmate_core::error::{ErrorKind, GatewayError};
use airmate_core::estimate::estimate_input_tokens;
use airmate_core::inbound::gemini::Call;
use airmate_core::inbound::InboundCtx;
use airmate_core::policy::PolicyConfig;

use super::generate::{fail, json_ok};
use super::{read_body, GwBody, Route};

/// The header value that marks a token count as a heuristic.
const ESTIMATE: &str = "heuristic";

/// `POST /v1beta/models/{model}:countTokens`: an estimate. The body is either `{"contents": …}` or
/// `{"generateContentRequest": {…}}`.
pub async fn count_tokens(route: Route, parts: Parts, body: Incoming) -> Response<GwBody> {
    let config = route.tables.config.clone();
    let body = match read_body(body, &parts.headers, config.server.max_body_bytes).await {
        Ok(b) => b,
        Err(e) => return fail(&route, e),
    };
    let Some(call) = route.gemini.clone() else {
        return fail(&route, GatewayError::not_served(parts.uri.path()));
    };
    // Unwrap `generateContentRequest`, and drop its `model` (the URL names it).
    let inner: Vec<u8> = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(serde_json::Value::Object(mut o)) => match o.remove("generateContentRequest") {
            Some(serde_json::Value::Object(mut g)) => {
                g.remove("model");
                serde_json::to_vec(&g).unwrap_or_default()
            }
            _ => body.to_vec(),
        },
        _ => body.to_vec(),
    };
    let ctx = InboundCtx {
        policy: PolicyConfig {
            unknown_fields: config.server.unknown_fields.into(),
        },
        signing_key: Some(&route.tables.signing_key),
        count_tokens: true,
    };
    match adapters::parse_gemini_request(
        &inner,
        &ctx,
        &Call {
            model: &call.model,
            stream: false,
        },
    ) {
        Ok(p) => json_ok(
            &route,
            serde_json::to_vec(
                &serde_json::json!({ "totalTokens": estimate_input_tokens(&p.value) }),
            )
            .expect("serialises"),
            &[("x-airmate-token-estimate", ESTIMATE.to_owned())],
        ),
        Err(e) => fail(&route, GatewayError::from_policy(&e)),
    }
}

fn names(route: &Route) -> Vec<String> {
    route
        .tables
        .models
        .entries()
        .iter()
        .filter(|e| !crate::router::glob::is_glob(&e.pattern))
        .map(|e| e.pattern.clone())
        .collect()
}

fn model_json(route: &Route, name: &str) -> serde_json::Value {
    let mut v = serde_json::json!({
        "name": format!("models/{name}"),
        "displayName": name,
        "supportedGenerationMethods": ["generateContent", "streamGenerateContent", "countTokens"],
    });
    if let Some(entry) = route
        .tables
        .models
        .entries()
        .iter()
        .find(|e| e.pattern == name)
    {
        if let Some(t) = entry.targets.first() {
            let caps = route.tables.catalog.for_target(t);
            if let Some(w) = caps.context_window {
                v["inputTokenLimit"] = serde_json::json!(w);
            }
            if let Some(o) = caps.max_output_tokens {
                v["outputTokenLimit"] = serde_json::json!(o);
            }
        }
    }
    v
}

/// `GET /v1beta/models`.
pub fn models(route: &Route) -> Response<GwBody> {
    let list: Vec<_> = names(route).iter().map(|n| model_json(route, n)).collect();
    json_ok(
        route,
        serde_json::to_vec(&serde_json::json!({ "models": list })).expect("serialises"),
        &[],
    )
}

/// `GET /v1beta/models/{model}`.
pub fn model(route: &Route, id: &str) -> Response<GwBody> {
    let id = id.strip_prefix("models/").unwrap_or(id);
    if names(route).iter().any(|n| n == id) {
        json_ok(
            route,
            serde_json::to_vec(&model_json(route, id)).expect("serialises"),
            &[],
        )
    } else {
        fail(
            route,
            GatewayError::new(
                ErrorKind::NotFound,
                format!("models/{id} is not found for API version v1beta"),
            ),
        )
    }
}
