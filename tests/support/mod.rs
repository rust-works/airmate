//! Test support: an in-tree mock upstream that scripts upstream behaviour (status codes, delays,
//! partial streams, `Retry-After`, resets), and a gateway started on a free port.
#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

use airmate::config::paths::EnvFn;
use airmate::config::{load_str, Config};
use airmate::obs::{RecordSink, RequestRecord};
use airmate::router::clock::{Clock, OsRandom, Random, SystemClock};
use airmate::secret::{Secret, SystemResolver};
use airmate::server::{bind_with_deps, Tables};

/// A request the mock saw.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the request body is JSON")
    }
}

/// One step of a scripted stream.
#[derive(Debug, Clone)]
pub enum Part {
    Bytes(Vec<u8>),
    Delay(Duration),
    /// Abort the connection mid-stream.
    Reset,
    /// Stop sending and keep the connection open forever.
    Hang,
}

/// What the mock answers.
#[derive(Debug, Clone)]
pub enum Reply {
    Json {
        status: u16,
        body: Vec<u8>,
        headers: Vec<(String, String)>,
    },
    Stream {
        status: u16,
        parts: Vec<Part>,
        headers: Vec<(String, String)>,
    },
    /// Do not answer for this long, then answer 200 `{}`.
    Slow(Duration),
}

impl Reply {
    pub fn json(status: u16, v: serde_json::Value) -> Self {
        Self::Json {
            status,
            body: serde_json::to_vec(&v).unwrap(),
            headers: vec![],
        }
    }

    pub fn json_with(status: u16, v: serde_json::Value, headers: &[(&str, &str)]) -> Self {
        Self::Json {
            status,
            body: serde_json::to_vec(&v).unwrap(),
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    pub fn sse(parts: Vec<Part>) -> Self {
        Self::Stream {
            status: 200,
            parts,
            headers: vec![("content-type".into(), "text/event-stream".into())],
        }
    }
}

struct ChannelBody(mpsc::Receiver<Result<Frame<Bytes>, std::io::Error>>);

impl Body for ChannelBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.0.poll_recv(cx)
    }
}

type Handler = dyn Fn(&Recorded) -> Reply + Send + Sync;

/// A scripted upstream server.
pub struct Mock {
    pub addr: SocketAddr,
    requests: Arc<Mutex<Vec<Recorded>>>,
    /// Streams whose receiver went away mid-stream: the gateway cancelled the upstream call.
    pub dropped: Arc<AtomicUsize>,
    /// Chunks of a scripted stream the mock managed to hand to the connection.
    pub sent: Arc<AtomicUsize>,
}

impl Mock {
    pub async fn start(handler: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let handler: Arc<Handler> = Arc::new(handler);
        let seen = requests.clone();
        let dropped = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(AtomicUsize::new(0));
        let (dropped2, sent2) = (dropped.clone(), sent.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let seen = seen.clone();
                let (dropped, sent) = (dropped2.clone(), sent2.clone());
                tokio::spawn(async move {
                    let svc = service_fn(move |req: Request<Incoming>| {
                        let handler = handler.clone();
                        let seen = seen.clone();
                        let (dropped, sent) = (dropped.clone(), sent.clone());
                        async move {
                            Ok::<_, Infallible>(answer(req, handler, seen, dropped, sent).await)
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await;
                });
            }
        });
        Self {
            addr,
            requests,
            dropped,
            sent,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn recorded(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

type MockBody = http_body_util::combinators::UnsyncBoxBody<Bytes, std::io::Error>;

async fn answer(
    req: Request<Incoming>,
    handler: Arc<Handler>,
    seen: Arc<Mutex<Vec<Recorded>>>,
    dropped: Arc<AtomicUsize>,
    sent: Arc<AtomicUsize>,
) -> Response<MockBody> {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();
    let rec = Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        headers: parts
            .headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned()))
            .collect(),
        body,
    };
    seen.lock().unwrap().push(rec.clone());
    match handler(&rec) {
        Reply::Json {
            status,
            body,
            headers,
        } => {
            let mut b = Response::builder()
                .status(status)
                .header("content-type", "application/json");
            for (k, v) in headers {
                b = b.header(k, v);
            }
            b.body(
                Full::new(Bytes::from(body))
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            )
            .unwrap()
        }
        Reply::Slow(d) => {
            tokio::time::sleep(d).await;
            Response::builder()
                .status(200)
                .body(
                    Full::new(Bytes::from_static(b"{}"))
                        .map_err(|never| match never {})
                        .boxed_unsync(),
                )
                .unwrap()
        }
        Reply::Stream {
            status,
            parts,
            headers,
        } => {
            let (tx, rx) = mpsc::channel(16);
            tokio::spawn(async move {
                for part in parts {
                    match part {
                        Part::Bytes(b) => {
                            if tx.send(Ok(Frame::data(Bytes::from(b)))).await.is_err() {
                                dropped.fetch_add(1, Ordering::SeqCst);
                                return;
                            }
                            sent.fetch_add(1, Ordering::SeqCst);
                        }
                        Part::Delay(d) => tokio::time::sleep(d).await,
                        Part::Reset => {
                            let _ = tx
                                .send(Err(std::io::Error::new(
                                    std::io::ErrorKind::ConnectionReset,
                                    "reset",
                                )))
                                .await;
                            return;
                        }
                        Part::Hang => {
                            std::future::pending::<()>().await;
                        }
                    }
                }
            });
            let mut b = Response::builder().status(status);
            for (k, v) in headers {
                b = b.header(k, v);
            }
            b.body(ChannelBody(rx).boxed_unsync()).unwrap()
        }
    }
}

/// A response seen by a test client.
#[derive(Debug)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// A gateway on a free port, with a known token.
pub struct TestGateway {
    pub addr: SocketAddr,
    pub token: String,
    pub config: Config,
    shutdown: watch::Sender<bool>,
    pub shared: Arc<airmate::server::Shared>,
}

impl TestGateway {
    /// Starts a gateway from `yaml`, with `server.port` forced to 0.
    pub async fn start(yaml: &str) -> Self {
        Self::start_with_env(yaml, &|_| None).await
    }

    pub async fn start_with_env(yaml: &str, env: EnvFn<'_>) -> Self {
        Self::start_with_sink(yaml, env, Arc::new(airmate::obs::NullSink)).await
    }

    pub async fn start_with_sink(yaml: &str, env: EnvFn<'_>, sink: Arc<dyn RecordSink>) -> Self {
        Self::start_full(
            yaml,
            env,
            sink,
            Arc::new(SystemClock::new()),
            Arc::new(OsRandom),
        )
        .await
    }

    /// Starts a gateway with the router's clock and randomness injected.
    pub async fn start_full(
        yaml: &str,
        env: EnvFn<'_>,
        sink: Arc<dyn RecordSink>,
        clock: Arc<dyn Clock>,
        random: Arc<dyn Random>,
    ) -> Self {
        let mut config = load_str(yaml, None).unwrap_or_else(|e| panic!("bad test config:\n{e}"));
        config.server.port = 0;
        let token = "test-gateway-token".to_owned();
        let (tables, _) = Tables::build(
            Arc::new(config.clone()),
            &SystemResolver::new(env),
            &Secret::new(token.clone()),
        )
        .unwrap_or_else(|e| panic!("tables: {e:?}"));
        let gw = bind_with_deps(&config, tables, sink, clock, random)
            .await
            .unwrap();
        let addr = gw.local_addr();
        let shared = gw.shared();
        let (shutdown, rx) = watch::channel(false);
        tokio::spawn(gw.serve(rx));
        Self {
            addr,
            token,
            config,
            shutdown,
            shared,
        }
    }

    pub async fn post(&self, path: &str, body: &serde_json::Value) -> Resp {
        self.request("POST", path, Some(body.to_string()), &[])
            .await
    }

    pub async fn post_with(
        &self,
        path: &str,
        body: &serde_json::Value,
        headers: &[(&str, &str)],
    ) -> Resp {
        self.request("POST", path, Some(body.to_string()), headers)
            .await
    }

    pub async fn get(&self, path: &str) -> Resp {
        self.request("GET", path, None, &[]).await
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<String>,
        headers: &[(&str, &str)],
    ) -> Resp {
        let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
            .build_http::<Full<Bytes>>();
        let mut b = Request::builder()
            .method(method)
            .uri(format!("http://{}{}", self.addr, path))
            .header("content-type", "application/json");
        // A test that brings its own credentials does not also get the default ones.
        let own_auth = headers.iter().any(|(k, _)| {
            k.eq_ignore_ascii_case("x-api-key") || k.eq_ignore_ascii_case("authorization")
        });
        if !own_auth {
            b = b.header("x-api-key", &self.token);
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = b
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .unwrap();
        let resp = client.request(req).await.expect("the gateway answers");
        let (parts, body) = resp.into_parts();
        let body = body
            .collect()
            .await
            .map(|c| c.to_bytes().to_vec())
            .unwrap_or_default();
        Resp {
            status: parts.status.as_u16(),
            headers: parts
                .headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_owned()))
                .collect(),
            body,
        }
    }

    /// Reloads the configuration from `yaml`, as `SIGHUP` would.
    pub fn reload(
        &self,
        yaml: &str,
    ) -> Result<airmate::server::reload::ReloadReport, Vec<airmate::config::ConfigError>> {
        let config = load_str(yaml, None).map_err(|e| e.0)?;
        let env: EnvFn<'_> = &|n| (n == "UP_KEY").then(|| "k".to_owned());
        airmate::server::reload::reload(
            &self.shared,
            config,
            &SystemResolver::new(env),
            &Secret::new(self.token.clone()),
        )
    }

    pub fn stop(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for TestGateway {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A config with one upstream (the mock) and a catch-all model entry.
pub fn yaml_for(
    mock_url: &str,
    protocol: &str,
    flavor: &str,
    extra_upstream: &str,
    extra_top: &str,
) -> String {
    format!(
        "upstreams:\n  - name: up\n    url: {mock_url}\n    protocol: {protocol}\n    flavor: {flavor}\n    api_key: {{ env: UP_KEY }}\n{extra_upstream}models:\n  - match: \"*\"\n    targets: [ {{ upstream: up, model: upstream-model }} ]\n{extra_top}"
    )
}

pub fn env_with_key(
    name: &'static str,
    value: &'static str,
) -> impl Fn(&str) -> Option<String> + Send + Sync {
    move |n| (n == name).then(|| value.to_owned())
}

/// Collects request-log records.
#[derive(Default)]
pub struct Capture(pub Mutex<Vec<RequestRecord>>);

impl RecordSink for Capture {
    fn record(&self, rec: &RequestRecord) {
        self.0.lock().unwrap().push(rec.clone());
    }
}

impl Capture {
    pub fn all(&self) -> Vec<RequestRecord> {
        self.0.lock().unwrap().clone()
    }

    /// Waits until at least `n` records have been written.
    pub async fn wait_for(&self, n: usize) -> Vec<RequestRecord> {
        for _ in 0..100 {
            if self.all().len() >= n {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        self.all()
    }
}
