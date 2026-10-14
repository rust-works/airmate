//! Gateway errors and their mapping.
//!
//! Every error is returned in the inbound protocol's envelope, with a status the official SDKs and
//! Claude Code already handle.

use serde_json::{json, Value as Json};

use crate::ir::Protocol;
use crate::outbound::UpstreamError;
use crate::policy::PolicyError;

/// The class of a gateway error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// `400 invalid_request_error`
    InvalidRequest,
    /// `401 authentication_error`: the client's own key is wrong.
    Authentication,
    /// `403 permission_error`
    Permission,
    /// `404 not_found_error`
    NotFound,
    /// `413 request_too_large`
    RequestTooLarge,
    /// `429 rate_limit_error`
    RateLimit,
    /// `504 timeout_error`
    Timeout,
    /// `529 overloaded_error`
    Overloaded,
    /// `500 api_error`
    Api,
    /// `502 api_error`: the upstream is unreachable.
    BadGateway,
}

impl ErrorKind {
    /// The HTTP status.
    #[must_use]
    pub fn status(self) -> u16 {
        match self {
            Self::InvalidRequest => 400,
            Self::Authentication => 401,
            Self::Permission => 403,
            Self::NotFound => 404,
            Self::RequestTooLarge => 413,
            Self::RateLimit => 429,
            Self::Timeout => 504,
            Self::Overloaded => 529,
            Self::Api => 500,
            Self::BadGateway => 502,
        }
    }

    /// The Anthropic `error.type`.
    #[must_use]
    pub fn anthropic_type(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request_error",
            Self::Authentication => "authentication_error",
            Self::Permission => "permission_error",
            Self::NotFound => "not_found_error",
            Self::RequestTooLarge => "request_too_large",
            Self::RateLimit => "rate_limit_error",
            Self::Timeout => "timeout_error",
            Self::Overloaded => "overloaded_error",
            Self::Api | Self::BadGateway => "api_error",
        }
    }

    /// The OpenAI `(error.type, error.code)`.
    #[must_use]
    pub fn openai_type_and_code(self) -> (&'static str, &'static str) {
        match self {
            Self::InvalidRequest => ("invalid_request_error", "invalid_request"),
            Self::Authentication => ("invalid_request_error", "invalid_api_key"),
            Self::Permission => ("permission_error", "permission_denied"),
            Self::NotFound => ("invalid_request_error", "not_found"),
            Self::RequestTooLarge => ("invalid_request_error", "request_too_large"),
            Self::RateLimit => ("rate_limit_error", "rate_limit_exceeded"),
            Self::Timeout => ("timeout_error", "timeout"),
            Self::Overloaded => ("server_error", "overloaded"),
            Self::Api | Self::BadGateway => ("server_error", "internal_error"),
        }
    }
}

/// An error to return to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayError {
    /// The class.
    pub kind: ErrorKind,
    /// The message for the client.
    pub message: String,
    /// A `Retry-After` in seconds to forward.
    pub retry_after: Option<u64>,
    /// The request id, also in the body.
    pub request_id: Option<String>,
    /// The request field the field policy refused, if this is a policy rejection.
    pub policy_field: Option<String>,
}

/// What names an upstream in a message.
#[derive(Debug, Clone, Copy)]
pub struct UpstreamLabel<'a> {
    /// The upstream's name.
    pub name: &'a str,
    /// The config setting that holds its key, e.g. `upstreams[fireworks].api_key`.
    pub key_setting: &'a str,
}

/// What is known about the size of the request, to word a context overflow.
#[derive(Debug, Clone, Copy, Default)]
pub struct SizeInfo {
    /// The target's context window, if the catalog knows it.
    pub context_window: Option<u64>,
    /// The estimated input tokens.
    pub est_input_tokens: u64,
    /// The output token limit.
    pub max_tokens: u32,
}

impl GatewayError {
    /// A new error.
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
            request_id: None,
            policy_field: None,
        }
    }

    /// Attaches the request id.
    #[must_use]
    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }

    /// Attaches a `Retry-After`.
    #[must_use]
    pub fn with_retry_after(mut self, seconds: Option<u64>) -> Self {
        self.retry_after = seconds;
        self
    }

    /// The HTTP status.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.kind.status()
    }

    /// `401 authentication_error`: the client's token is missing or wrong.
    #[must_use]
    pub fn unauthenticated() -> Self {
        Self::new(
            ErrorKind::Authentication,
            "invalid or missing airmate token",
        )
    }

    /// `403 permission_error` from the Host/Origin guard.
    #[must_use]
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Permission, message)
    }

    /// `404 not_found_error` for an endpoint that is not served.
    #[must_use]
    pub fn not_served(path: &str) -> Self {
        Self::new(
            ErrorKind::NotFound,
            format!("`{path}` is not served by airmate"),
        )
    }

    /// `429 rate_limit_error` for the gateway's own concurrency limit.
    #[must_use]
    pub fn at_capacity(retry_after: u64) -> Self {
        Self::new(
            ErrorKind::RateLimit,
            "airmate is at its concurrency limit; retry shortly",
        )
        .with_retry_after(Some(retry_after))
    }

    /// `502 api_error` for an upstream that cannot be reached.
    #[must_use]
    pub fn unreachable(upstream: &str, detail: &str) -> Self {
        Self::new(
            ErrorKind::BadGateway,
            format!("could not reach the upstream `{upstream}`: {detail}"),
        )
    }

    /// `504 timeout_error` for an upstream that did not answer in time.
    #[must_use]
    pub fn upstream_timeout(upstream: &str) -> Self {
        Self::new(
            ErrorKind::Timeout,
            format!("the upstream `{upstream}` timed out"),
        )
    }

    /// A policy rejection: `400 invalid_request_error` naming the field.
    #[must_use]
    pub fn from_policy(e: &PolicyError) -> Self {
        let mut err = Self::new(ErrorKind::InvalidRequest, e.message.clone());
        err.policy_field = Some(e.field.clone());
        err
    }

    /// Maps an upstream answer.
    #[must_use]
    pub fn from_upstream(
        e: &UpstreamError,
        upstream: UpstreamLabel<'_>,
        size: SizeInfo,
        retry_after: Option<u64>,
    ) -> Self {
        let (kind, message) = match e.status {
            400 | 422 if e.is_context_overflow() => {
                (ErrorKind::InvalidRequest, context_overflow_message(e, size))
            }
            401 | 403 => (
                ErrorKind::Permission,
                format!(
                    "the upstream `{}` rejected its API key ({}); check `{}`",
                    upstream.name, e.message, upstream.key_setting
                ),
            ),
            404 => (ErrorKind::NotFound, e.message.clone()),
            408 | 504 => (ErrorKind::Timeout, e.message.clone()),
            413 => (ErrorKind::RequestTooLarge, e.message.clone()),
            429 => (ErrorKind::RateLimit, e.message.clone()),
            503 | 529 => (ErrorKind::Overloaded, e.message.clone()),
            500..=599 => (ErrorKind::Api, e.message.clone()),
            _ => (ErrorKind::InvalidRequest, e.message.clone()),
        };
        let mut err = Self::new(kind, message);
        if kind == ErrorKind::RateLimit {
            err.retry_after = retry_after;
        }
        err
    }

    /// The status and body in `inbound`'s envelope.
    #[must_use]
    pub fn render(&self, inbound: Protocol) -> (u16, Vec<u8>) {
        let body: Json = match inbound {
            Protocol::Anthropic | Protocol::Gemini | Protocol::Bedrock => {
                let mut v = json!({"type": "error", "error": {
                    "type": self.kind.anthropic_type(),
                    "message": self.message,
                }});
                if let Some(id) = &self.request_id {
                    v["request_id"] = json!(id);
                }
                v
            }
            Protocol::Chat | Protocol::Responses => {
                let (ty, code) = self.kind.openai_type_and_code();
                json!({"error": {"message": self.message, "type": ty, "code": code, "param": null}})
            }
        };
        (
            self.status(),
            serde_json::to_vec(&body).expect("a JSON value serialises"),
        )
    }

    /// Response headers: `retry-after` and `request-id`.
    #[must_use]
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = Vec::new();
        if let Some(s) = self.retry_after {
            h.push(("retry-after", s.to_string()));
        }
        if let Some(id) = &self.request_id {
            h.push(("request-id", id.clone()));
        }
        h
    }
}

/// Parses the OpenAI-style overflow message
/// `maximum context length is N tokens. However, you requested T tokens (M in the messages, C in the completion)`.
fn parse_overflow(message: &str) -> Option<(u64, u64, u64)> {
    fn number_before(haystack: &str, marker: &str) -> Option<u64> {
        let end = haystack.find(marker)?;
        let digits: String = haystack[..end]
            .trim_end()
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        digits.parse().ok()
    }
    let window = {
        let at = message.find("maximum context length is")? + "maximum context length is".len();
        let digits: String = message[at..]
            .trim_start()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse().ok()?
    };
    Some((
        window,
        number_before(message, "in the messages")?,
        number_before(message, "in the completion")?,
    ))
}

fn context_overflow_message(e: &UpstreamError, size: SizeInfo) -> String {
    let (window, input) = match (size.context_window, parse_overflow(&e.message)) {
        (Some(w), _) => (Some(w), size.est_input_tokens),
        (None, Some((w, m, _))) => (Some(w), m),
        (None, None) => (None, size.est_input_tokens),
    };
    if let Some(w) = window {
        if input < w && input + u64::from(size.max_tokens) > w {
            return format!(
                "the prompt fits the context window ({input} of {w} tokens) but `max_tokens` \
                 ({}) on top of it does not; lower `max_tokens`",
                size.max_tokens
            );
        }
    }
    format!("prompt is too long: {}", e.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(status: u16, message: &str) -> UpstreamError {
        UpstreamError {
            status,
            message: message.into(),
            code: None,
        }
    }

    fn label() -> UpstreamLabel<'static> {
        UpstreamLabel {
            name: "fireworks",
            key_setting: "upstreams[fireworks].api_key",
        }
    }

    fn map(status: u16, message: &str) -> GatewayError {
        GatewayError::from_upstream(&up(status, message), label(), SizeInfo::default(), Some(7))
    }

    #[test]
    fn the_status_table_matches_the_specification() {
        let table = [
            (400, ErrorKind::InvalidRequest, 400),
            (422, ErrorKind::InvalidRequest, 400),
            (401, ErrorKind::Permission, 403),
            (403, ErrorKind::Permission, 403),
            (404, ErrorKind::NotFound, 404),
            (408, ErrorKind::Timeout, 504),
            (504, ErrorKind::Timeout, 504),
            (413, ErrorKind::RequestTooLarge, 413),
            (429, ErrorKind::RateLimit, 429),
            (503, ErrorKind::Overloaded, 529),
            (529, ErrorKind::Overloaded, 529),
            (500, ErrorKind::Api, 500),
            (502, ErrorKind::Api, 500),
            (418, ErrorKind::InvalidRequest, 400),
        ];
        for (from, kind, status) in table {
            let e = map(from, "msg");
            assert_eq!((e.kind, e.status()), (kind, status), "upstream {from}");
        }
    }

    #[test]
    fn an_upstream_401_is_never_a_401_to_the_client() {
        let e = map(401, "Incorrect API key provided");
        assert_eq!(e.status(), 403);
        assert!(
            e.message.contains("`fireworks`") && e.message.contains("upstreams[fireworks].api_key")
        );
        assert!(e.message.contains("Incorrect API key"));
    }

    #[test]
    fn retry_after_is_forwarded_only_for_rate_limits() {
        assert_eq!(map(429, "slow").retry_after, Some(7));
        assert_eq!(map(500, "boom").retry_after, None);
        assert_eq!(
            map(429, "slow").headers(),
            vec![("retry-after", "7".to_string())]
        );
        assert!(map(500, "x").headers().is_empty());
    }

    #[test]
    fn context_overflow_is_reworded_so_claude_code_compacts() {
        let e = map(400, "This model's maximum context length is 8192 tokens");
        assert!(e.message.starts_with("prompt is too long"), "{}", e.message);
        assert_eq!(e.kind, ErrorKind::InvalidRequest);
        let by_code = GatewayError::from_upstream(
            &UpstreamError {
                status: 400,
                message: "x".into(),
                code: Some("context_length_exceeded".into()),
            },
            label(),
            SizeInfo::default(),
            None,
        );
        assert!(by_code.message.starts_with("prompt is too long"));
    }

    #[test]
    fn when_the_prompt_fits_but_max_tokens_does_not_the_message_says_so() {
        let known = SizeInfo {
            context_window: Some(8192),
            est_input_tokens: 5000,
            max_tokens: 4000,
        };
        let e = GatewayError::from_upstream(
            &up(400, "maximum context length exceeded"),
            label(),
            known,
            None,
        );
        assert!(
            e.message.contains("lower `max_tokens`")
                && !e.message.starts_with("prompt is too long"),
            "{}",
            e.message
        );
        let too_big = SizeInfo {
            context_window: Some(8192),
            est_input_tokens: 9000,
            max_tokens: 100,
        };
        assert!(GatewayError::from_upstream(
            &up(400, "maximum context length exceeded"),
            label(),
            too_big,
            None
        )
        .message
        .starts_with("prompt is too long"));
    }

    #[test]
    fn the_openai_overflow_message_can_stand_in_for_a_missing_window() {
        let msg = "This model's maximum context length is 8192 tokens. However, you requested 9000 tokens (5000 in the messages, 4000 in the completion). Please reduce the length.";
        let sized = SizeInfo {
            context_window: None,
            est_input_tokens: 1,
            max_tokens: 4000,
        };
        let e = GatewayError::from_upstream(&up(400, msg), label(), sized, None);
        assert!(
            e.message.contains("5000 of 8192") && e.message.contains("lower `max_tokens`"),
            "{}",
            e.message
        );
        let both = "This model's maximum context length is 8192 tokens. However, you requested 20000 tokens (16000 in the messages, 4000 in the completion).";
        assert!(
            GatewayError::from_upstream(&up(400, both), label(), sized, None)
                .message
                .starts_with("prompt is too long")
        );
        assert_eq!(parse_overflow("nothing useful"), None);
        assert_eq!(parse_overflow("maximum context length is 10 tokens"), None);
    }

    #[test]
    fn gateway_originated_errors_have_the_specified_classes() {
        assert_eq!(GatewayError::unauthenticated().status(), 401);
        assert_eq!(GatewayError::forbidden("bad host").status(), 403);
        assert_eq!(GatewayError::not_served("/v1/files").status(), 404);
        assert!(GatewayError::not_served("/v1/files")
            .message
            .contains("/v1/files"));
        let cap = GatewayError::at_capacity(2);
        assert_eq!((cap.status(), cap.retry_after), (429, Some(2)));
        assert_eq!(
            GatewayError::unreachable("local", "connection refused").status(),
            502
        );
        assert_eq!(GatewayError::upstream_timeout("local").status(), 504);
        let policy = GatewayError::from_policy(&PolicyError {
            field: "safeguards".into(),
            message: "`safeguards` is not supported".into(),
        });
        assert_eq!(
            (policy.status(), policy.message.as_str()),
            (400, "`safeguards` is not supported")
        );
    }

    #[test]
    fn anthropic_errors_use_the_anthropic_envelope_with_the_request_id() {
        let (status, body) = GatewayError::new(ErrorKind::RateLimit, "slow")
            .with_request_id("req_1")
            .render(Protocol::Anthropic);
        assert_eq!(status, 429);
        let v: Json = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            v,
            json!({"type": "error", "error": {"type": "rate_limit_error", "message": "slow"}, "request_id": "req_1"})
        );
        let (_, body) = GatewayError::new(ErrorKind::Api, "x").render(Protocol::Anthropic);
        assert!(serde_json::from_slice::<Json>(&body)
            .unwrap()
            .get("request_id")
            .is_none());
    }

    #[test]
    fn openai_errors_use_the_openai_error_object() {
        for p in [Protocol::Chat, Protocol::Responses] {
            let (status, body) = GatewayError::new(ErrorKind::Authentication, "no").render(p);
            assert_eq!(status, 401);
            let v: Json = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                v,
                json!({"error": {"message": "no", "type": "invalid_request_error", "code": "invalid_api_key", "param": null}})
            );
        }
    }

    #[test]
    fn every_kind_has_a_status_an_anthropic_type_and_an_openai_pair() {
        use ErrorKind::*;
        for k in [
            InvalidRequest,
            Authentication,
            Permission,
            NotFound,
            RequestTooLarge,
            RateLimit,
            Timeout,
            Overloaded,
            Api,
            BadGateway,
        ] {
            assert!(k.status() >= 400);
            assert!(
                k.anthropic_type().ends_with("_error") || k.anthropic_type() == "request_too_large"
            );
            let (t, c) = k.openai_type_and_code();
            assert!(!t.is_empty() && !c.is_empty());
        }
    }

    #[test]
    fn request_ids_ride_in_the_headers() {
        let e = GatewayError::new(ErrorKind::Api, "x")
            .with_request_id("req_9")
            .with_retry_after(Some(3));
        assert_eq!(
            e.headers(),
            vec![
                ("retry-after", "3".to_string()),
                ("request-id", "req_9".to_string())
            ]
        );
    }
}
