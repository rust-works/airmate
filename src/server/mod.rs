//! The HTTP server: listener, request guard and routing.

pub mod guard;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::http::request::Parts;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::TcpListener;
use tokio::sync::watch;

use airmate_core::error::GatewayError;
use airmate_core::ir::Protocol;

use crate::auth::{Authenticator, Identity};
use crate::config::schema::Config;
use guard::{check_host, check_origin, extract_token, GuardConfig};

/// The response body type: either a buffered body or a stream.
pub type GwBody = UnsyncBoxBody<Bytes, Infallible>;

/// A fully buffered body.
#[must_use]
pub fn full(bytes: impl Into<Bytes>) -> GwBody {
    Full::new(bytes.into()).boxed_unsync()
}

/// A request id: the client's `x-request-id` when it is sane, else a fresh one.
#[must_use]
pub fn request_id(headers: &hyper::HeaderMap) -> String {
    if let Some(v) = headers.get("x-request-id").and_then(|v| v.to_str().ok()) {
        if !v.is_empty() && v.len() <= 128 && v.bytes().all(|b| b.is_ascii_graphic()) {
            return v.to_owned();
        }
    }
    let mut raw = [0u8; 12];
    let _ = getrandom::fill(&mut raw);
    format!(
        "req_{}",
        raw.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// The protocol whose error envelope an answer to `path` uses.
#[must_use]
pub fn protocol_for_path(path: &str) -> Protocol {
    if path.starts_with("/v1/chat/completions") {
        Protocol::Chat
    } else if path.starts_with("/v1/responses") {
        Protocol::Responses
    } else {
        Protocol::Anthropic
    }
}

/// An error as an HTTP response in `inbound`'s envelope.
#[must_use]
pub fn error_response(inbound: Protocol, e: &GatewayError) -> Response<GwBody> {
    let (status, body) = e.render(inbound);
    let mut b = Response::builder()
        .status(status)
        .header("content-type", "application/json");
    for (k, v) in e.headers() {
        b = b.header(k, v);
    }
    b.body(full(body)).expect("a valid response")
}

/// What can change on reload: the configuration and what is derived from it.
pub struct Tables {
    /// The configuration in force.
    pub config: Arc<Config>,
    /// Authenticates presented tokens.
    pub auth: Box<dyn Authenticator>,
}

/// State shared by every connection.
pub struct Shared {
    /// The listener facts the guard needs; restart-only.
    pub guard: GuardConfig,
    tables: RwLock<Arc<Tables>>,
}

impl Shared {
    /// The tables in force right now. In-flight requests keep the tables they started with.
    ///
    /// # Panics
    ///
    /// If a writer panicked while holding the lock.
    #[must_use]
    pub fn tables(&self) -> Arc<Tables> {
        self.tables.read().expect("tables lock").clone()
    }

    /// Atomically swaps the tables.
    ///
    /// # Panics
    ///
    /// If a writer panicked while holding the lock.
    pub fn swap_tables(&self, tables: Tables) {
        *self.tables.write().expect("tables lock") = Arc::new(tables);
    }
}

/// A gateway bound to a socket, ready to serve.
pub struct BoundGateway {
    listener: TcpListener,
    shared: Arc<Shared>,
    addr: SocketAddr,
}

/// Why binding failed.
#[derive(Debug)]
pub struct BindError(pub String);

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BindError {}

/// Binds the listener. Fails closed: if the port is taken there is no fallback to another one.
///
/// # Errors
///
/// The address is invalid or cannot be bound.
pub async fn bind(config: &Config, tables: Tables) -> Result<BoundGateway, BindError> {
    let addr: SocketAddr = format!(
        "{}:{}",
        if config.server.bind.contains(':') {
            format!("[{}]", config.server.bind)
        } else {
            config.server.bind.clone()
        },
        config.server.port
    )
    .parse()
    .map_err(|e| BindError(format!("invalid listen address: {e}")))?;
    let listener = TcpListener::bind(addr).await.map_err(|e| {
        BindError(format!(
            "cannot listen on {addr}: {e}{}",
            if e.kind() == std::io::ErrorKind::AddrInUse {
                "; airmate does not fall back to another port (use --port 0 to ask for any free port)"
            } else {
                ""
            }
        ))
    })?;
    let local = listener
        .local_addr()
        .map_err(|e| BindError(e.to_string()))?;
    let shared = Arc::new(Shared {
        guard: GuardConfig {
            bound_port: Some(local.port()),
            allowed_hosts: config.server.allowed_hosts.clone(),
        },
        tables: RwLock::new(Arc::new(tables)),
    });
    Ok(BoundGateway {
        listener,
        shared,
        addr: local,
    })
}

impl BoundGateway {
    /// The address actually bound (useful with port 0).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The shared state, for reload.
    #[must_use]
    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Accepts connections until `shutdown` is set.
    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) {
        let mut backoff = Duration::from_millis(5);
        loop {
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok((stream, _)) => {
                        backoff = Duration::from_millis(5);
                        let shared = self.shared.clone();
                        tokio::spawn(async move { serve_connection(stream, shared).await });
                    }
                    Err(e) => {
                        // EMFILE and friends: back off instead of spinning.
                        eprintln!("airmate: accept failed: {e}; retrying in {backoff:?}");
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(1));
                    }
                },
                _ = shutdown.changed() => break,
            }
        }
    }
}

async fn serve_connection(stream: tokio::net::TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let service = service_fn(move |req| {
        let shared = shared.clone();
        async move { Ok::<_, Infallible>(handle(shared, req).await) }
    });
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30))
        .keep_alive(true);
    let _ = http.serve_connection(TokioIo::new(stream), service).await;
}

/// Runs the guard: Host, Origin and Sec-Fetch-Site, then the token.
///
/// # Errors
///
/// `403` for the first two checks, `401` for the token.
pub fn run_guard(
    shared: &Shared,
    tables: &Tables,
    parts: &Parts,
) -> Result<Identity, GatewayError> {
    let header = |name: &str| parts.headers.get(name).and_then(|v| v.to_str().ok());
    check_host(header("host"), &shared.guard)?;
    check_origin(header("origin"), header("sec-fetch-site"))?;
    let token = extract_token(header("x-api-key"), header("authorization"));
    token
        .and_then(|t| tables.auth.authenticate(t))
        .ok_or_else(GatewayError::unauthenticated)
}

async fn handle(shared: Arc<Shared>, req: Request<Incoming>) -> Response<GwBody> {
    let (parts, _body) = req.into_parts();
    let rid = request_id(&parts.headers);
    let inbound = protocol_for_path(parts.uri.path());
    let tables = shared.tables();
    let _identity = match run_guard(&shared, &tables, &parts) {
        Ok(id) => id,
        Err(e) => return error_response(inbound, &e.with_request_id(rid)),
    };
    let e = GatewayError::not_served(parts.uri.path()).with_request_id(rid);
    error_response(inbound, &e)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TokenAuthenticator;
    use crate::secret::Secret;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) fn tables(token: &str) -> Tables {
        Tables {
            config: Arc::new(Config::default()),
            auth: Box::new(TokenAuthenticator::new(&Secret::new(token), &[]).unwrap()),
        }
    }

    pub(crate) async fn start(
        token: &str,
        mutate: impl FnOnce(&mut Config),
    ) -> (SocketAddr, watch::Sender<bool>) {
        let mut config = Config::default();
        config.server.port = 0;
        mutate(&mut config);
        let gw = bind(&config, tables(token)).await.unwrap();
        let addr = gw.local_addr();
        let (tx, rx) = watch::channel(false);
        tokio::spawn(gw.serve(rx));
        (addr, tx)
    }

    /// A raw HTTP/1.1 exchange with `Connection: close`; returns (status, headers, body).
    pub(crate) async fn raw(addr: SocketAddr, request: &str) -> (u16, String, String) {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        (status, head.to_owned(), body.to_owned())
    }

    pub(crate) fn get(addr: SocketAddr, path: &str, extra: &str) -> String {
        format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n{extra}\r\n",
            addr.port()
        )
    }

    #[tokio::test]
    async fn requests_without_a_token_get_401_and_say_nothing_else() {
        let (addr, _stop) = start("tok", |_| {}).await;
        for path in ["/", "/v1/messages", "/anything/else"] {
            let (status, _, body) = raw(addr, &get(addr, path, "")).await;
            assert_eq!(status, 401, "{path}");
            assert!(body.contains("authentication_error"), "{body}");
            assert!(
                !body.contains(path),
                "an unauthenticated probe learns nothing about paths"
            );
        }
        let (status, _, _) = raw(
            addr,
            &format!(
                "HEAD / HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
                addr.port()
            ),
        )
        .await;
        assert_eq!(status, 401, "liveness probes get 401");
    }

    #[tokio::test]
    async fn a_wrong_token_is_401_and_a_right_one_reaches_the_router() {
        let (addr, _stop) = start("tok", |_| {}).await;
        let (s, _, _) = raw(addr, &get(addr, "/v1/messages", "x-api-key: nope\r\n")).await;
        assert_eq!(s, 401);
        let (s, _, body) = raw(addr, &get(addr, "/nowhere", "x-api-key: tok\r\n")).await;
        assert_eq!(s, 404);
        assert!(
            body.contains("not_found_error") && body.contains("/nowhere"),
            "{body}"
        );
        let (s, _, _) = raw(
            addr,
            &get(addr, "/nowhere", "Authorization: Bearer tok\r\n"),
        )
        .await;
        assert_eq!(s, 404, "a bearer token works too");
    }

    #[tokio::test]
    async fn the_host_and_origin_guards_run_before_the_token_check() {
        let (addr, _stop) = start("tok", |_| {}).await;
        let rebinding = format!("GET /v1/models HTTP/1.1\r\nHost: evil.example:{}\r\nx-api-key: tok\r\nConnection: close\r\n\r\n", addr.port());
        let (s, _, body) = raw(addr, &rebinding).await;
        assert_eq!(s, 403, "DNS rebinding is refused even with a valid token");
        assert!(body.contains("permission_error"));
        let (s, _, _) = raw(
            addr,
            &get(
                addr,
                "/v1/models",
                "Origin: https://evil.example\r\nx-api-key: tok\r\n",
            ),
        )
        .await;
        assert_eq!(s, 403, "a web page cannot reach the gateway");
        let (s, _, _) = raw(
            addr,
            &get(
                addr,
                "/v1/models",
                "Sec-Fetch-Site: cross-site\r\nx-api-key: tok\r\n",
            ),
        )
        .await;
        assert_eq!(s, 403);
        let (s, _, _) = raw(
            addr,
            &get(addr, "/v1/models", "Origin: https://evil.example\r\n"),
        )
        .await;
        assert_eq!(s, 403, "the origin check comes before the token check");
        let (s, _, _) = raw(
            addr,
            "GET / HTTP/1.1\r\nHost: 127.0.0.1:1\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert_eq!(s, 403, "the wrong port in Host");
    }

    #[tokio::test]
    async fn error_envelopes_follow_the_path_family_and_carry_the_request_id() {
        let (addr, _stop) = start("tok", |_| {}).await;
        let (s, head, body) = raw(
            addr,
            &get(addr, "/v1/chat/completions", "x-request-id: abc-123\r\n"),
        )
        .await;
        assert_eq!(s, 401);
        assert!(
            body.contains("\"error\"") && body.contains("invalid_api_key"),
            "OpenAI envelope: {body}"
        );
        assert!(
            !body.contains("request_id"),
            "OpenAI bodies have no request_id"
        );
        assert!(head
            .to_ascii_lowercase()
            .contains("content-type: application/json"));
        let (_, _, body) = raw(
            addr,
            &get(addr, "/v1/messages", "x-request-id: abc-123\r\n"),
        )
        .await;
        assert!(body.contains("\"request_id\":\"abc-123\""), "{body}");
        let (_, _, body) = raw(addr, &get(addr, "/v1/messages", "")).await;
        assert!(
            body.contains("\"request_id\":\"req_"),
            "a fresh id is minted: {body}"
        );
    }

    #[tokio::test]
    async fn the_port_is_taken_means_failure_not_a_fallback() {
        let (addr, _stop) = start("tok", |_| {}).await;
        let mut config = Config::default();
        config.server.port = addr.port();
        let err = match bind(&config, tables("tok")).await {
            Ok(_) => panic!("binding a taken port must fail"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("cannot listen") && err.contains("does not fall back"),
            "{err}"
        );
        config.server.bind = "not-an-ip".into();
        assert!(bind(&config, tables("tok")).await.is_err());
    }

    #[tokio::test]
    async fn port_zero_picks_a_free_port_and_the_default_is_9997() {
        let (addr, _stop) = start("tok", |_| {}).await;
        assert_ne!(addr.port(), 0);
        assert_eq!(Config::default().server.port, 9997);
    }

    #[tokio::test]
    async fn ipv6_loopback_binds_too() {
        let mut config = Config::default();
        config.server.port = 0;
        config.server.bind = "::1".into();
        // Some CI sandboxes have no IPv6; only assert on success.
        if let Ok(gw) = bind(&config, tables("tok")).await {
            assert!(gw.local_addr().ip().is_loopback());
        }
    }

    #[tokio::test]
    async fn shutdown_stops_the_accept_loop() {
        let mut config = Config::default();
        config.server.port = 0;
        let gw = bind(&config, tables("t")).await.unwrap();
        let (tx, rx) = watch::channel(false);
        let handle = tokio::spawn(gw.serve(rx));
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("serve returns after shutdown")
            .unwrap();
    }

    #[test]
    fn request_ids_are_echoed_when_sane_and_minted_otherwise() {
        let mut h = hyper::HeaderMap::new();
        assert!(request_id(&h).starts_with("req_"));
        h.insert("x-request-id", "abc".parse().unwrap());
        assert_eq!(request_id(&h), "abc");
        h.insert("x-request-id", "x".repeat(200).parse().unwrap());
        assert!(
            request_id(&h).starts_with("req_"),
            "an oversized id is replaced"
        );
        h.insert("x-request-id", "has space".parse().unwrap());
        assert!(request_id(&h).starts_with("req_"));
    }

    #[test]
    fn paths_pick_their_error_family() {
        assert_eq!(protocol_for_path("/v1/chat/completions"), Protocol::Chat);
        assert_eq!(protocol_for_path("/v1/responses"), Protocol::Responses);
        assert_eq!(protocol_for_path("/v1/messages"), Protocol::Anthropic);
        assert_eq!(protocol_for_path("/anything"), Protocol::Anthropic);
    }

    #[tokio::test]
    async fn tables_swap_atomically() {
        let mut config = Config::default();
        config.server.port = 0;
        let gw = bind(&config, tables("old")).await.unwrap();
        let shared = gw.shared();
        let before = shared.tables();
        assert!(before.auth.authenticate("old").is_some());
        shared.swap_tables(tables("new"));
        assert!(
            shared.tables().auth.authenticate("new").is_some()
                && shared.tables().auth.authenticate("old").is_none()
        );
        assert!(
            before.auth.authenticate("old").is_some(),
            "an in-flight request keeps the tables it started with"
        );
    }
}
