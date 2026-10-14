//! Calling upstreams.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use hyper::{Method, Request, StatusCode};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};

use airmate_core::error::GatewayError;
use airmate_core::ir::Protocol;
use airmate_core::outbound::UpstreamRequest;

use crate::router::UpstreamRt;

/// A pooled HTTP(S) client for one upstream.
pub type HttpClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// The default `anthropic-version` sent when the client sent none.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Builds a client with its own connection pool and connect timeout.
#[must_use]
pub fn build_client(connect_timeout: Duration) -> HttpClient {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    http.set_connect_timeout(Some(connect_timeout));
    let https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(60))
        .build(https)
}

/// An upstream's answer, with the body still streaming.
pub struct UpstreamResponse {
    /// The status.
    pub status: StatusCode,
    /// The headers.
    pub headers: HeaderMap,
    /// The body.
    pub body: Incoming,
}

/// Why a call produced no response.
#[derive(Debug)]
pub enum SendError {
    /// No response headers within the first-byte timeout.
    Timeout,
    /// The connection failed.
    Connect(String),
}

impl SendError {
    /// The error to return to the client.
    #[must_use]
    pub fn into_gateway(self, upstream: &str) -> GatewayError {
        match self {
            Self::Timeout => GatewayError::upstream_timeout(upstream),
            Self::Connect(detail) => GatewayError::unreachable(upstream, &detail),
        }
    }
}

fn header_value(s: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(s).ok()
}

/// Sends `req` to `up`. The upstream key is attached here and nowhere else.
///
/// # Errors
///
/// The connection failed or no response arrived within the first-byte timeout.
pub async fn send(
    up: &UpstreamRt,
    req: &UpstreamRequest,
    client_headers: &HeaderMap,
) -> Result<UpstreamResponse, SendError> {
    let uri = format!("{}{}", up.base_url, req.path);
    let mut b = Request::builder().method(Method::POST).uri(uri);
    {
        let h = b.headers_mut().expect("a fresh builder has headers");
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(
            ACCEPT,
            HeaderValue::from_static(match (up.protocol, req.stream) {
                (Protocol::Bedrock, true) => "application/vnd.amazon.eventstream",
                (_, true) => "text/event-stream",
                (_, false) => "application/json",
            }),
        );
        h.insert(
            USER_AGENT,
            HeaderValue::from_static(concat!("airmate/", env!("CARGO_PKG_VERSION"))),
        );
        match up.protocol {
            Protocol::Anthropic => {
                if let Some(k) = up.api_key.as_ref().and_then(|k| header_value(k.expose())) {
                    h.insert("x-api-key", k);
                }
                let version = client_headers
                    .get("anthropic-version")
                    .cloned()
                    .unwrap_or(HeaderValue::from_static(ANTHROPIC_VERSION));
                h.insert("anthropic-version", version);
                if let Some(beta) = client_headers.get("anthropic-beta") {
                    h.insert("anthropic-beta", beta.clone());
                }
            }
            Protocol::Gemini => {
                if let Some(k) = up.api_key.as_ref().and_then(|k| header_value(k.expose())) {
                    h.insert("x-goog-api-key", k);
                }
            }
            Protocol::Bedrock if up.aws.is_some() => {
                sign_bedrock(up, req, h);
            }
            _ => {
                if let Some(k) = up
                    .api_key
                    .as_ref()
                    .and_then(|k| header_value(&format!("Bearer {}", k.expose())))
                {
                    h.insert(AUTHORIZATION, k);
                }
            }
        }
    }
    let request = b
        .body(Full::new(Bytes::from(req.body.clone())))
        .map_err(|e| SendError::Connect(e.to_string()))?;
    let first_byte = Duration::from_secs(up.timeouts.first_byte_secs);
    match tokio::time::timeout(first_byte, up.client.request(request)).await {
        Err(_) => Err(SendError::Timeout),
        Ok(Err(e)) => Err(SendError::Connect(describe(&e))),
        Ok(Ok(resp)) => {
            let (parts, body) = resp.into_parts();
            Ok(UpstreamResponse {
                status: parts.status,
                headers: parts.headers,
                body,
            })
        }
    }
}

/// Adds the Signature Version 4 headers for a Bedrock request. The key material is used here and
/// nowhere else.
fn sign_bedrock(up: &UpstreamRt, req: &UpstreamRequest, h: &mut HeaderMap) {
    use airmate_core::outbound::sigv4::{amz_date, sign, Credentials, Request as SignRequest};
    let Some(aws) = &up.aws else { return };
    let (path, query) = req.path.split_once('?').unwrap_or((&req.path, ""));
    let authority = up
        .base_url
        .split_once("://")
        .map_or(up.base_url.as_str(), |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let date = amz_date(now);
    let signed = sign(
        &Credentials {
            access_key_id: aws.access_key_id.expose(),
            secret_access_key: aws.secret_access_key.expose(),
            session_token: aws
                .session_token
                .as_ref()
                .map(crate::secret::Secret::expose),
        },
        &SignRequest {
            method: "POST",
            host: authority,
            path,
            query,
            body: &req.body,
            region: &aws.region,
            service: "bedrock",
            amz_date: &date,
        },
    );
    for (k, v) in signed {
        if let (Ok(name), Some(value)) = (
            hyper::header::HeaderName::from_bytes(k.as_bytes()),
            header_value(&v),
        ) {
            h.insert(name, value);
        }
    }
}

fn describe(e: &hyper_util::client::legacy::Error) -> String {
    let mut s = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(inner) = source {
        s.push_str(": ");
        s.push_str(&inner.to_string());
        source = inner.source();
    }
    s
}

/// The `Retry-After` header as whole seconds, if it is a number.
#[must_use]
pub fn retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_reads_seconds_only() {
        let mut h = HeaderMap::new();
        assert_eq!(retry_after(&h), None);
        h.insert("retry-after", "30".parse().unwrap());
        assert_eq!(retry_after(&h), Some(30));
        h.insert("retry-after", " 7 ".parse().unwrap());
        assert_eq!(retry_after(&h), Some(7));
        h.insert(
            "retry-after",
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after(&h), None, "HTTP dates are not interpreted");
    }

    #[test]
    fn send_errors_become_gateway_errors() {
        assert_eq!(SendError::Timeout.into_gateway("u").status(), 504);
        let e = SendError::Connect("refused".into()).into_gateway("u");
        assert_eq!(e.status(), 502);
        assert!(e.message.contains("`u`") && e.message.contains("refused"));
    }

    #[test]
    fn a_client_builds_with_tls_roots() {
        let _ = build_client(Duration::from_secs(1));
    }
}
