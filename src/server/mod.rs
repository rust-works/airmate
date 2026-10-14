//! The HTTP server: listener, request guard and routing.

pub mod anthropic;
pub mod gemini;
pub mod generate;
pub mod guard;
pub mod listener;
pub mod record;
pub mod reload;
pub mod stream;
pub mod upstream;

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
use tokio::sync::{watch, Semaphore};

use airmate_core::error::GatewayError;
use airmate_core::ir::Protocol;
use airmate_core::signature::SigningKey;

use crate::auth::{Authenticator, Identity};
use crate::config::schema::Config;
use crate::config::validate::ConfigError;
use crate::obs::{NullSink, RecordSink};
use crate::router::catalog::Catalog;
use crate::router::clock::{OsRandom, SystemClock};
use crate::router::rules::{Action, Classifier, CompiledRule, RuleSet};
use crate::router::state::RouterState;
use crate::router::ModelTable;
use crate::secret::{Secret, SecretResolver};
use crate::usage::{Budgets, NullUsage, UsageSink};
use guard::{check_host, check_origin, extract_token, GuardConfig};
pub use listener::{BindError, Listener};

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

/// Whether `path` is a Gemini method call such as `/v1/models/gemini-2.5-pro:generateContent`.
fn is_gemini_method(path: &str) -> bool {
    path.starts_with("/v1/models/") && path.contains(':')
}

/// What the URL of a Gemini request says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeminiCall {
    /// The model in the path.
    pub model: String,
    /// The method: `generateContent`, `streamGenerateContent` or `countTokens`.
    pub method: String,
}

impl GeminiCall {
    /// Reads `/v1beta/models/{model}:{method}` (or the same under `/v1/`).
    #[must_use]
    pub fn parse(path: &str) -> Option<Self> {
        let rest = path
            .strip_prefix("/v1beta/models/")
            .or_else(|| path.strip_prefix("/v1/models/"))?;
        let (model, method) = rest.split_once(':')?;
        (!model.is_empty()
            && !model.contains('/')
            && matches!(
                method,
                "generateContent" | "streamGenerateContent" | "countTokens"
            ))
        .then(|| Self {
            model: model.to_owned(),
            method: method.to_owned(),
        })
    }

    /// Whether the method streams.
    #[must_use]
    pub fn streams(&self) -> bool {
        self.method == "streamGenerateContent"
    }
}

/// The protocol whose error envelope an answer to `path` uses.
#[must_use]
pub fn protocol_for_path(path: &str) -> Protocol {
    if path.starts_with("/v1/chat/completions") || path.starts_with("/v1/embeddings") {
        Protocol::Chat
    } else if path.starts_with("/v1beta/") || is_gemini_method(path) {
        Protocol::Gemini
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
    /// The upstreams and model entries.
    pub models: ModelTable,
    /// Signs and verifies thinking blocks; derived from the gateway token.
    pub signing_key: SigningKey,
    /// What each model can do.
    pub catalog: Catalog,
    /// The routing rules and the classifiers they consult.
    pub rules: RuleSet,
    /// The named clients, by name.
    pub clients: std::collections::HashMap<String, crate::config::schema::ClientConfig>,
    /// Prices for models whose target and catalog entry carry none.
    pub prices: crate::usage::PriceTable,
    /// Secrets scrubbed from any text that goes to a client, a log or the status page.
    redactions: Vec<Secret>,
}

impl Tables {
    /// `data` with every upstream key and the gateway token replaced by `[redacted]`, or `None`
    /// when it held none.
    #[must_use]
    pub fn redact_bytes(&self, data: &[u8]) -> Option<Vec<u8>> {
        crate::secret::redact_bytes(data, self.redactions.iter().map(Secret::expose))
    }

    /// `text` with every upstream key and the gateway token replaced by `[redacted]`.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        crate::secret::redact(text, self.redactions.iter().map(Secret::expose))
    }

    /// Builds the tables from a configuration, resolving every secret.
    ///
    /// # Errors
    ///
    /// Every secret that cannot be resolved, and a malformed client key.
    pub fn build(
        config: Arc<Config>,
        resolver: &dyn SecretResolver,
        gateway_token: &Secret,
    ) -> Result<(Self, Vec<String>), Vec<ConfigError>> {
        Self::build_with(config, resolver, gateway_token, Arc::new(NullSink))
    }

    /// Like [`Tables::build`], logging classifier calls to `sink`.
    ///
    /// # Errors
    ///
    /// Every secret that cannot be resolved, and a malformed client key.
    pub fn build_with(
        config: Arc<Config>,
        resolver: &dyn SecretResolver,
        gateway_token: &Secret,
        sink: Arc<dyn RecordSink>,
    ) -> Result<(Self, Vec<String>), Vec<ConfigError>> {
        let (models, warnings) = ModelTable::build(&config, resolver)?;
        let auth = crate::auth::TokenAuthenticator::new(gateway_token, &config.clients)
            .map_err(|e| vec![ConfigError::new("clients", e.to_string())])?;
        let rules = compile_rules(&config, &models, sink);
        let redactions: Vec<Secret> = models
            .upstreams()
            .flat_map(|u| {
                let aws = u.aws.iter().flat_map(|a| {
                    [
                        Some(a.access_key_id.clone()),
                        Some(a.secret_access_key.clone()),
                        a.session_token.clone(),
                    ]
                    .into_iter()
                    .flatten()
                });
                u.api_key.clone().into_iter().chain(aws)
            })
            .chain(std::iter::once(gateway_token.clone()))
            .collect();
        let prices = crate::usage::PriceTable::load(&config.usage.prices)
            .map_err(|e| vec![ConfigError::new("usage.prices", e)])?;
        Ok((
            Self {
                prices,
                redactions,
                catalog: Catalog::new(&config.catalog),
                rules,
                clients: config
                    .clients
                    .iter()
                    .map(|c| (c.name.clone(), c.clone()))
                    .collect(),
                config,
                auth: Box::new(auth),
                models,
                signing_key: SigningKey::from_token(gateway_token.expose()),
            },
            warnings,
        ))
    }
}

/// Turns the configured rules into rules that can be evaluated.
fn compile_rules(config: &Config, models: &ModelTable, sink: Arc<dyn RecordSink>) -> RuleSet {
    let rules = config
        .rules
        .iter()
        .filter_map(|r| {
            let action = match (&r.use_entry, &r.targets) {
                (Some(name), _) => Action::Entry(name.clone()),
                (None, Some(targets)) => Action::Targets(
                    targets
                        .iter()
                        .filter_map(|t| {
                            Some(crate::router::RouteTarget {
                                upstream: models.upstream(&t.upstream)?.clone(),
                                model: t.model.clone(),
                                effort_levels: t
                                    .effort_levels
                                    .as_ref()
                                    .map(|l| l.iter().map(|e| (*e).into()).collect()),
                                max_output_tokens: t.max_output_tokens,
                                weight: t.weight.unwrap_or(1),
                                price: t.price,
                            })
                        })
                        .collect(),
                ),
                (None, None) => return None,
            };
            Some(CompiledRule {
                name: r.name.clone(),
                when: r.when.clone(),
                action,
            })
        })
        .collect();
    let classifiers: std::collections::HashMap<String, Arc<dyn Classifier>> = config
        .classifiers
        .iter()
        .map(|c| {
            (
                c.name.clone(),
                Arc::new(crate::router::classifier::HttpClassifier::new(
                    c,
                    sink.clone(),
                )) as Arc<dyn Classifier>,
            )
        })
        .collect();
    RuleSet::new(rules, classifiers)
}

/// State shared by every connection.
pub struct Shared {
    /// The listener facts the guard needs; restart-only.
    pub guard: GuardConfig,
    /// The global concurrency limit.
    pub permits: Arc<Semaphore>,
    /// Where request records go.
    pub log: Arc<dyn RecordSink>,
    /// What the router has learned: breakers, latency, affinity.
    pub router: Arc<RouterState>,
    /// Rate limiters for clients and upstreams.
    pub limits: crate::router::limits::Limits,
    /// Where usage ledger lines go.
    pub usage: Arc<dyn UsageSink>,
    /// Spend per client, for budgets.
    pub budgets: Budgets,
    /// Where request and response bodies go, when body logging is on.
    pub bodies: Arc<dyn crate::bodylog::BodySink>,
    /// Counters and histograms behind `/metrics`.
    pub metrics: Arc<crate::metrics::Metrics>,
    /// When the gateway started.
    pub started: std::time::Instant,
    /// Where the gateway listens, as a URL (`http://127.0.0.1:9997`, `unix:/path`).
    pub listen: String,
    /// Set when draining has run out of time: streams still open end with an error event.
    pub hard_stop: watch::Sender<bool>,
    /// How long shutdown lets in-flight requests finish.
    pub shutdown_timeout: Duration,
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
    listener: Listener,
    shared: Arc<Shared>,
    addr: SocketAddr,
    #[cfg(feature = "tls")]
    tls: Option<tokio_rustls::TlsAcceptor>,
}

/// Binds the listener. Fails closed: if the port is taken there is no fallback to another one.
///
/// # Errors
///
/// The address is invalid or cannot be bound.
pub async fn bind(config: &Config, tables: Tables) -> Result<BoundGateway, BindError> {
    bind_with(config, tables, Arc::new(NullSink)).await
}

/// Like [`bind`], writing request records to `log`.
///
/// # Errors
///
/// The address is invalid or cannot be bound.
pub async fn bind_with(
    config: &Config,
    tables: Tables,
    log: Arc<dyn RecordSink>,
) -> Result<BoundGateway, BindError> {
    bind_with_deps(
        config,
        tables,
        log,
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom),
    )
    .await
}

/// Like [`bind_with`], with the clock and the source of randomness the router uses injected, so
/// breakers, backoff and weighted choice can be driven deterministically.
///
/// # Errors
///
/// The address is invalid or cannot be bound.
pub async fn bind_with_deps(
    config: &Config,
    tables: Tables,
    log: Arc<dyn RecordSink>,
    clock: Arc<dyn crate::router::clock::Clock>,
    random: Arc<dyn crate::router::clock::Random>,
) -> Result<BoundGateway, BindError> {
    let listener = Listener::bind(&config.server).await?;
    bind_listener(config, listener, tables, log, clock, random)
}

/// Like [`bind_with_deps`] on a listener that is already open: a Unix-domain socket, or a socket a
/// service manager passed in.
///
/// # Errors
///
/// TLS material that cannot be read, or TLS configured in a build without the `tls` feature.
pub fn bind_listener(
    config: &Config,
    listener: Listener,
    tables: Tables,
    log: Arc<dyn RecordSink>,
    clock: Arc<dyn crate::router::clock::Clock>,
    random: Arc<dyn crate::router::clock::Random>,
) -> Result<BoundGateway, BindError> {
    #[cfg(feature = "tls")]
    let tls = config
        .server
        .tls
        .as_ref()
        .map(listener::tls_acceptor)
        .transpose()?;
    #[cfg(not(feature = "tls"))]
    if config.server.tls.is_some() {
        return Err(BindError(
            "`server.tls` is set but this build of airmate has no `tls` feature".to_owned(),
        ));
    }
    let local = listener.local_addr();
    let metrics = crate::metrics::Metrics::new();
    let (hard_stop, _) = watch::channel(false);
    let shared = Arc::new(Shared {
        guard: GuardConfig {
            bound_port: listener.port(),
            allowed_hosts: config.server.allowed_hosts.clone(),
        },
        permits: Arc::new(Semaphore::new(config.server.max_concurrent as usize)),
        log: Arc::new(crate::metrics::MeteredSink::new(log, metrics.clone())),
        metrics,
        started: std::time::Instant::now(),
        #[cfg(feature = "tls")]
        listen: listener.describe(tls.is_some()),
        #[cfg(not(feature = "tls"))]
        listen: listener.describe(false),
        hard_stop,
        shutdown_timeout: Duration::from_secs(config.server.shutdown_timeout_secs),
        limits: crate::router::limits::Limits::new(clock.clone()),
        usage: Arc::new(NullUsage),
        bodies: Arc::new(crate::bodylog::NullBodies),
        budgets: Budgets::new(),
        router: Arc::new(RouterState::new(
            clock,
            random,
            (&config.circuit).into(),
            10_000,
        )),
        tables: RwLock::new(Arc::new(tables)),
    });
    Ok(BoundGateway {
        listener,
        shared,
        addr: local,
        #[cfg(feature = "tls")]
        tls,
    })
}

impl BoundGateway {
    /// The address actually bound (useful with port 0).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Records usage to `sink` and starts the budgets from `history`, the ledger as it stands.
    ///
    /// # Panics
    ///
    /// If called after [`BoundGateway::shared`] has handed the shared state out.
    #[must_use]
    pub fn with_usage(
        mut self,
        sink: Arc<dyn UsageSink>,
        history: &[crate::usage::UsageRecord],
    ) -> Self {
        let shared = Arc::get_mut(&mut self.shared)
            .expect("with_usage must be called before the shared state is handed out");
        shared.usage = sink;
        shared.budgets = Budgets::rebuild(history);
        self
    }

    /// Logs request and response bodies to `sink`.
    ///
    /// # Panics
    ///
    /// If called after [`BoundGateway::shared`] has handed the shared state out.
    #[must_use]
    pub fn with_bodies(mut self, sink: Arc<dyn crate::bodylog::BodySink>) -> Self {
        Arc::get_mut(&mut self.shared)
            .expect("with_bodies must be called before the shared state is handed out")
            .bodies = sink;
        self
    }

    /// The shared state, for reload.
    #[must_use]
    pub fn shared(&self) -> Arc<Shared> {
        self.shared.clone()
    }

    /// Where it listens, as a URL.
    #[must_use]
    pub fn listen_url(&self) -> String {
        self.shared.listen.clone()
    }

    /// Accepts connections until `shutdown` is set, then drains: no new connections, idle ones
    /// close, and requests in flight get `server.shutdown_timeout_secs` to finish. Streams still
    /// open after that end with an `error` event rather than being cut off.
    pub async fn serve(self, mut shutdown: watch::Receiver<bool>) {
        tokio::spawn(crate::router::health::run(
            self.shared.clone(),
            shutdown.clone(),
        ));
        let graceful = hyper_util::server::graceful::GracefulShutdown::new();
        let mut backoff = Duration::from_millis(5);
        let mut last_alert: Option<std::time::Instant> = None;
        loop {
            tokio::select! {
                accepted = self.listener.accept() => match accepted {
                    Ok(conn) => {
                        backoff = Duration::from_millis(5);
                        self.spawn_connection(conn, &graceful);
                    }
                    Err(e) => {
                        // EMFILE and friends: back off instead of spinning, and say so at most
                        // once a minute rather than on every retry.
                        if last_alert.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                            eprintln!("airmate: accept failed: {e}; backing off");
                            last_alert = Some(std::time::Instant::now());
                        }
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(1));
                    }
                },
                _ = shutdown.changed() => break,
            }
        }
        self.listener.cleanup();
        let drain = graceful.shutdown();
        tokio::pin!(drain);
        let grace = self.shared.shutdown_timeout;
        if tokio::time::timeout(grace, &mut drain).await.is_err() {
            eprintln!(
                "airmate: {} second(s) passed with requests still open; ending their streams",
                grace.as_secs()
            );
            let _ = self.shared.hard_stop.send(true);
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut drain).await;
        }
    }

    fn spawn_connection(
        &self,
        conn: listener::Accepted,
        graceful: &hyper_util::server::graceful::GracefulShutdown,
    ) {
        let shared = self.shared.clone();
        match conn {
            listener::Accepted::Tcp(stream) => {
                let _ = stream.set_nodelay(true);
                #[cfg(feature = "tls")]
                if let Some(acceptor) = self.tls.clone() {
                    let watcher = graceful.watcher();
                    tokio::spawn(async move {
                        // A handshake that never finishes must not hold a task forever.
                        let hs =
                            tokio::time::timeout(Duration::from_secs(10), acceptor.accept(stream));
                        if let Ok(Ok(tls)) = hs.await {
                            serve_connection_watched(tls, shared, watcher).await;
                        }
                    });
                    return;
                }
                let watcher = graceful.watcher();
                tokio::spawn(serve_connection_watched(stream, shared, watcher));
            }
            #[cfg(unix)]
            listener::Accepted::Unix(stream) => {
                let watcher = graceful.watcher();
                tokio::spawn(serve_connection_watched(stream, shared, watcher));
            }
        }
    }
}

async fn serve_connection_watched<S>(
    stream: S,
    shared: Arc<Shared>,
    watcher: hyper_util::server::graceful::Watcher,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = service_fn(move |req| {
        let shared = shared.clone();
        async move { Ok::<_, Infallible>(handle(shared, req).await) }
    });
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(30))
        .keep_alive(true);
    let _ = watcher
        .watch(http.serve_connection(TokioIo::new(stream), service))
        .await;
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
    let token = extract_token(
        header("x-api-key").or_else(|| header("x-goog-api-key")),
        header("authorization"),
    );
    token
        .and_then(|t| tables.auth.authenticate(t))
        .ok_or_else(GatewayError::unauthenticated)
}

async fn handle(shared: Arc<Shared>, req: Request<Incoming>) -> Response<GwBody> {
    let (parts, body) = req.into_parts();
    let rid = request_id(&parts.headers);
    let inbound = protocol_for_path(parts.uri.path());
    let tables = shared.tables();
    let identity = match run_guard(&shared, &tables, &parts) {
        Ok(id) => id,
        Err(e) => {
            // Refused requests are logged too: they are the interesting ones.
            let rec = record::Recorder::new(shared.log.clone(), &rid, "-", inbound);
            rec.finish_response(e.status());
            return error_response(inbound, &e.with_request_id(rid));
        }
    };
    let rec = record::Recorder::new(shared.log.clone(), &rid, &identity.client, inbound);
    let route = Route {
        shared,
        tables,
        identity,
        rid: rid.clone(),
        inbound,
        rec: rec.clone(),
        endpoint: if parts.uri.path() == "/v1/embeddings" {
            Endpoint::Embeddings
        } else {
            Endpoint::Generate
        },
        gemini: GeminiCall::parse(parts.uri.path()),
    };
    let path = parts.uri.path().to_owned();
    let resp = match (parts.method.as_str(), path.as_str()) {
        ("POST", "/v1/messages" | "/v1/chat/completions" | "/v1/responses" | "/v1/embeddings") => {
            generate::generate(route, parts, body).await
        }
        ("POST", _) if route.gemini.is_some() => {
            let call = route.gemini.clone().expect("checked above");
            if call.streams()
                && !parts
                    .uri
                    .query()
                    .is_some_and(|q| q.split('&').any(|p| p == "alt=sse"))
            {
                generate::fail(
                    &route,
                    GatewayError::new(
                        airmate_core::error::ErrorKind::InvalidRequest,
                        "airmate streams Gemini responses as server-sent events: add `alt=sse` to the URL",
                    ),
                )
            } else if call.method == "countTokens" {
                gemini::count_tokens(route, parts, body).await
            } else {
                generate::generate(route, parts, body).await
            }
        }
        ("GET", "/v1beta/models") => gemini::models(&route),
        ("GET", p) if p.starts_with("/v1beta/models/") => {
            gemini::model(&route, &p["/v1beta/models/".len()..])
        }
        ("POST", "/v1/messages/count_tokens") => anthropic::count_tokens(route, parts, body).await,
        ("GET", "/v1/models") => anthropic::models(&route, &parts),
        ("GET", p) if p.starts_with("/v1/models/") => {
            anthropic::model(&route, &parts, &p["/v1/models/".len()..])
        }
        _ => error_response(
            inbound,
            &GatewayError::not_served(&path).with_request_id(rid),
        ),
    };
    rec.finish_response(resp.status().as_u16());
    resp
}

/// What a handler knows about the request it is serving.
pub struct Route {
    /// The shared state.
    pub shared: Arc<Shared>,
    /// The tables the request started with.
    pub tables: Arc<Tables>,
    /// Who is calling.
    pub identity: Identity,
    /// The request id.
    pub rid: String,
    /// The protocol the client speaks.
    pub inbound: Protocol,
    /// Accumulates the request-log record.
    pub rec: record::Recorder,
    /// What the request asks for.
    pub endpoint: Endpoint,
    /// For a Gemini method call, what its URL says.
    pub gemini: Option<GeminiCall>,
}

/// What a request to the generation handler asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// A conversation: Messages, Chat Completions or Responses.
    Generate,
    /// Embeddings.
    Embeddings,
}

impl Route {
    /// Writes a body to the body log, if that is on.
    pub fn log_body(&self, direction: &str, status: u16, bytes: &[u8]) {
        let bodies = &self.shared.bodies;
        if bodies.enabled() {
            bodies.record(&crate::bodylog::make_record(
                &self.rid,
                &self.identity.client,
                direction,
                status,
                bytes,
                bodies.max_bytes(),
            ));
        }
    }
}

/// Reads a request body, refusing more than `max` bytes.
///
/// # Errors
///
/// `413 request_too_large` over the cap, `400` if the body cannot be read.
pub async fn read_body(
    body: Incoming,
    headers: &hyper::HeaderMap,
    max: u64,
) -> Result<Bytes, GatewayError> {
    let declared = headers
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let too_large = || {
        GatewayError::new(
            airmate_core::error::ErrorKind::RequestTooLarge,
            format!("the request body is larger than {max} bytes"),
        )
    };
    if declared.is_some_and(|n| n > max) {
        return Err(too_large());
    }
    match http_body_util::Limited::new(body, usize::try_from(max).unwrap_or(usize::MAX))
        .collect()
        .await
    {
        Ok(c) => Ok(c.to_bytes()),
        Err(e)
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some() =>
        {
            Err(too_large())
        }
        Err(_) => Err(GatewayError::new(
            airmate_core::error::ErrorKind::InvalidRequest,
            "the request body could not be read",
        )),
    }
}

/// Takes a concurrency slot, queueing for `queue_timeout` if configured.
///
/// # Errors
///
/// `429 rate_limit_error` when none is free in time.
pub async fn acquire(
    shared: &Shared,
    queue_timeout: Option<std::time::Duration>,
) -> Result<tokio::sync::OwnedSemaphorePermit, GatewayError> {
    let permits = shared.permits.clone();
    match permits.clone().try_acquire_owned() {
        Ok(p) => Ok(p),
        Err(_) => match queue_timeout {
            Some(t) => match tokio::time::timeout(t, permits.acquire_owned()).await {
                Ok(Ok(p)) => Ok(p),
                _ => Err(GatewayError::at_capacity(1)),
            },
            None => Err(GatewayError::at_capacity(1)),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub(crate) fn tables(token: &str) -> Tables {
        let env: crate::config::paths::EnvFn<'_> = &|_| None;
        Tables::build(
            Arc::new(Config::default()),
            &crate::secret::SystemResolver::new(env),
            &Secret::new(token),
        )
        .unwrap()
        .0
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
