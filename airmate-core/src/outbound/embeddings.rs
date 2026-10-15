//! Embeddings: `POST /v1/embeddings` in the OpenAI shape, forwarded to an upstream that speaks an
//! OpenAI protocol.
//!
//! Embeddings carry no conversation, so there is nothing to translate: the model is mapped, the
//! fields are checked against the short list the API defines, and the reply goes back with the
//! model the client named. A target whose protocol has no embeddings endpoint is not a candidate.

use serde_json::{Map, Value as Json};

use super::anthropic::estimate_value;
use super::{Target, UpstreamRequest};
use crate::facts::Facts;
use crate::ir::{Protocol, Usage};
use crate::policy::PolicyError;
use crate::toolname::ToolNames;

/// The embeddings path.
pub const PATH: &str = "/v1/embeddings";

/// The request fields the API defines.
const KNOWN: [&str; 5] = ["model", "input", "encoding_format", "dimensions", "user"];

fn bad(field: &str, message: impl Into<String>) -> PolicyError {
    PolicyError {
        field: field.to_owned(),
        message: message.into(),
    }
}

fn object(body: &[u8]) -> Result<Map<String, Json>, PolicyError> {
    match serde_json::from_slice::<Json>(body) {
        Ok(Json::Object(o)) => Ok(o),
        Ok(_) => Err(bad("body", "the request body must be a JSON object")),
        Err(e) => Err(bad(
            "body",
            format!("the request body is not valid JSON: {e}"),
        )),
    }
}

/// Reads what routing needs from an embeddings request and checks it is well formed.
///
/// # Errors
///
/// The body is not a JSON object with a `model` and a non-empty `input`, or has a field the API
/// does not define.
pub fn peek(body: &[u8]) -> Result<Facts, PolicyError> {
    let o = object(body)?;
    if let Some(unknown) = o.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(bad(
            unknown,
            format!("the field `{unknown}` is not known to airmate for embeddings"),
        ));
    }
    let model = o
        .get("model")
        .and_then(Json::as_str)
        .filter(|m| !m.is_empty())
        .ok_or_else(|| bad("model", "`model` is required"))?
        .to_owned();
    let input = o
        .get("input")
        .ok_or_else(|| bad("input", "`input` is required"))?;
    let empty = match input {
        Json::String(s) => s.is_empty(),
        Json::Array(a) => a.is_empty(),
        _ => true,
    };
    if empty {
        return Err(bad(
            "input",
            "`input` must be a string, or a non-empty array of strings or token arrays",
        ));
    }
    if let Some(f) = o.get("encoding_format").and_then(Json::as_str) {
        if f != "float" && f != "base64" {
            return Err(bad(
                "encoding_format",
                "`encoding_format` must be `float` or `base64`",
            ));
        }
    }
    Ok(Facts {
        inbound: Protocol::Chat,
        model,
        stream: false,
        tools: false,
        images: false,
        thinking: false,
        structured_output: false,
        effort: None,
        est_input_tokens: estimate_value(input),
        max_tokens: 0,
    })
}

/// Builds the forwarded request for `target`.
///
/// # Errors
///
/// The body is invalid (see [`peek`]), or the target's protocol has no embeddings endpoint.
pub fn build(body: &[u8], target: &Target) -> Result<UpstreamRequest, PolicyError> {
    if !matches!(target.protocol, Protocol::Chat | Protocol::Responses) {
        return Err(bad(
            "model",
            format!(
                "the upstream `{}` speaks the {} protocol, which has no embeddings endpoint",
                target.upstream,
                target.protocol.as_str()
            ),
        ));
    }
    peek(body)?;
    let mut o = object(body)?;
    o.insert("model".to_owned(), Json::String(target.model.clone()));
    Ok(UpstreamRequest {
        path: PATH.to_owned(),
        body: serde_json::to_vec(&Json::Object(o)).expect("a JSON value serialises"),
        stream: false,
        tool_names: ToolNames::new(),
        ignored: Vec::new(),
    })
}

/// Rewrites the reply so it names the client's model, and reads its usage. A reply that is not a
/// JSON object is returned unchanged.
#[must_use]
pub fn rewrite_response(body: &[u8], client_model: &str) -> (Vec<u8>, Usage) {
    let Ok(Json::Object(mut o)) = serde_json::from_slice::<Json>(body) else {
        return (body.to_vec(), Usage::default());
    };
    let tokens = o
        .get("usage")
        .and_then(|u| u.get("prompt_tokens").or_else(|| u.get("total_tokens")))
        .and_then(Json::as_u64)
        .unwrap_or(0);
    if o.contains_key("model") {
        o.insert("model".to_owned(), Json::String(client_model.to_owned()));
    }
    (
        serde_json::to_vec(&Json::Object(o)).expect("a JSON value serialises"),
        Usage {
            input_tokens: tokens,
            ..Usage::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;
    use serde_json::json;

    fn t(protocol: Protocol) -> Target {
        Target::new("up", protocol, Flavor::Generic, "text-embedding-3-small")
    }

    fn b(v: &Json) -> Vec<u8> {
        v.to_string().into_bytes()
    }

    #[test]
    fn peek_reads_the_model_and_sizes_the_input() {
        let f = peek(&b(
            &json!({"model": "embed", "input": "twelve chars", "dimensions": 256}),
        ))
        .unwrap();
        assert_eq!(f.model, "embed");
        assert!(f.est_input_tokens >= 3 && !f.stream && !f.tools);
        let many = peek(&b(&json!({"model": "embed", "input": ["a", "bb", "ccc"]}))).unwrap();
        assert!(many.est_input_tokens >= 1);
        assert!(peek(&b(
            &json!({"model": "embed", "input": [[1, 2, 3]], "encoding_format": "base64"})
        ))
        .is_ok());
    }

    #[test]
    fn malformed_requests_are_refused_naming_the_field() {
        for (body, field) in [
            (json!({"input": "x"}), "model"),
            (json!({"model": "", "input": "x"}), "model"),
            (json!({"model": "m"}), "input"),
            (json!({"model": "m", "input": ""}), "input"),
            (json!({"model": "m", "input": []}), "input"),
            (json!({"model": "m", "input": 5}), "input"),
            (
                json!({"model": "m", "input": "x", "encoding_format": "hex"}),
                "encoding_format",
            ),
            (
                json!({"model": "m", "input": "x", "stream": true}),
                "stream",
            ),
        ] {
            let e = peek(&b(&body)).unwrap_err();
            assert_eq!(e.field, field, "{body}: {}", e.message);
        }
        assert_eq!(peek(b"[]").unwrap_err().field, "body");
        assert_eq!(peek(b"nope").unwrap_err().field, "body");
    }

    #[test]
    fn the_model_is_mapped_and_the_rest_forwarded() {
        let req = build(
            &b(&json!({"model": "client-name", "input": "hi", "dimensions": 64, "user": "u"})),
            &t(Protocol::Chat),
        )
        .unwrap();
        assert_eq!(req.path, "/v1/embeddings");
        assert!(!req.stream);
        let sent: Json = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(
            sent,
            json!({"model": "text-embedding-3-small", "input": "hi", "dimensions": 64, "user": "u"})
        );
        assert!(build(
            &b(&json!({"model": "m", "input": "x"})),
            &t(Protocol::Responses)
        )
        .is_ok());
    }

    #[test]
    fn a_target_without_an_embeddings_api_is_not_a_candidate() {
        for p in [Protocol::Anthropic, Protocol::Gemini, Protocol::Bedrock] {
            let e = build(&b(&json!({"model": "m", "input": "x"})), &t(p)).unwrap_err();
            assert!(
                e.message.contains("no embeddings endpoint") && e.message.contains(p.as_str()),
                "{}",
                e.message
            );
        }
    }

    #[test]
    fn the_reply_names_the_clients_model_and_reports_usage() {
        let reply = json!({"object": "list", "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}], "model": "text-embedding-3-small", "usage": {"prompt_tokens": 7, "total_tokens": 7}});
        let (out, usage) = rewrite_response(&b(&reply), "my-embedder");
        let v: Json = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "my-embedder");
        assert_eq!(v["data"], reply["data"]);
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(
            rewrite_response(b"not json", "m"),
            (b"not json".to_vec(), Usage::default())
        );
    }
}
