//! Inbound adapters: parse a client's request into the IR, classing every field.

pub mod anthropic;
pub mod chat;
pub mod responses;

use serde_json::{Map, Value as Json};

use crate::policy::{PolicyConfig, PolicyError};
use crate::signature::SigningKey;

/// Everything an inbound parser needs besides the body.
#[derive(Debug, Clone, Copy, Default)]
pub struct InboundCtx<'a> {
    /// Policy settings.
    pub policy: PolicyConfig,
    /// The key that verifies thinking signatures; without one every thinking block is unverified.
    pub signing_key: Option<&'a SigningKey>,
    /// A `count_tokens` request: `max_tokens` is optional because nothing is generated.
    pub count_tokens: bool,
}

pub(crate) fn invalid(field: impl Into<String>, message: impl Into<String>) -> PolicyError {
    PolicyError {
        field: field.into(),
        message: message.into(),
    }
}

pub(crate) fn parse_object(body: &[u8]) -> Result<Map<String, Json>, PolicyError> {
    match serde_json::from_slice::<Json>(body) {
        Ok(Json::Object(m)) => Ok(m),
        Ok(_) => Err(invalid("body", "the request body must be a JSON object")),
        Err(e) => Err(invalid(
            "body",
            format!("the request body is not valid JSON: {e}"),
        )),
    }
}

pub(crate) fn want_object<'a>(
    v: &'a Json,
    path: &str,
) -> Result<&'a Map<String, Json>, PolicyError> {
    v.as_object()
        .ok_or_else(|| invalid(path, format!("`{path}` must be an object")))
}

pub(crate) fn want_array<'a>(v: &'a Json, path: &str) -> Result<&'a Vec<Json>, PolicyError> {
    v.as_array()
        .ok_or_else(|| invalid(path, format!("`{path}` must be an array")))
}

pub(crate) fn want_str<'a>(v: &'a Json, path: &str) -> Result<&'a str, PolicyError> {
    v.as_str()
        .ok_or_else(|| invalid(path, format!("`{path}` must be a string")))
}

pub(crate) fn want_bool(v: &Json, path: &str) -> Result<bool, PolicyError> {
    v.as_bool()
        .ok_or_else(|| invalid(path, format!("`{path}` must be a boolean")))
}

pub(crate) fn want_f64(v: &Json, path: &str) -> Result<f64, PolicyError> {
    v.as_f64()
        .ok_or_else(|| invalid(path, format!("`{path}` must be a number")))
}

pub(crate) fn want_u32(v: &Json, path: &str) -> Result<u32, PolicyError> {
    v.as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| invalid(path, format!("`{path}` must be a non-negative integer")))
}

pub(crate) fn want_i64(v: &Json, path: &str) -> Result<i64, PolicyError> {
    v.as_i64()
        .ok_or_else(|| invalid(path, format!("`{path}` must be an integer")))
}
