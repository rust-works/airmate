//! `airmate serve`: the gateway in the foreground.

use std::io::Write;
use std::sync::Arc;

use tokio::sync::watch;

use super::common::{gateway_token, load, CliError, Global, Process};
use crate::bodylog::{BodySink, JsonlBodies, NullBodies};
use crate::config::overrides::Overrides;
use crate::obs::{JsonlLog, NullSink, RecordSink, Rotation};
use crate::router::clock::{OsRandom, SystemClock};
use crate::secret::SystemResolver;
use crate::server::reload::{reload, ReloadReport};
use crate::server::{bind_listener, bind_with, Listener, Tables};
use crate::usage::{NullUsage, UsageRecord, UsageSink};

/// Runs the gateway until interrupted.
///
/// # Errors
///
/// A configuration problem, an unresolvable secret, or a port that cannot be bound.
pub fn serve(
    p: &Process,
    global: &Global,
    flags: Overrides,
    err: &mut dyn Write,
) -> Result<(), CliError> {
    let flags_for_reload = flags.clone();
    let loaded = load(p, global, flags)?;
    let token = gateway_token(p, &loaded)?;
    let config = Arc::new(loaded.resolved.config.clone());
    let resolver = SystemResolver::new(&*p.env);
    let (tables, warnings) = Tables::build(config.clone(), &resolver, &token)
        .map_err(|errs| CliError::usage(crate::config::ConfigErrors(errs).to_string()))?;
    for w in &warnings {
        let _ = writeln!(err, "airmate: warning: {w}");
    }
    let sink = request_log(&config, &loaded.dirs.request_log(), err);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::runtime(format!("cannot start the async runtime: {e}")))?;
    rt.block_on(async {
        // Handlers go in before the gateway is announced: a SIGHUP that arrives first would
        // otherwise take the process down with the default action.
        let mut signals = Signals::new();
        let sink = with_otel(&config, sink, &resolver, err);
        let (usage_sink, history) = usage_ledger(&config, &loaded.dirs.usage_ledger(), err);
        // Plenty of descriptors for concurrent streams; the default soft limit is often 256.
        let _ = crate::sys::raise_fd_limit(4096);
        let gw = match crate::sys::inherited_listener() {
            Some(socket) => {
                let _ = writeln!(
                    err,
                    "airmate: using the socket the service manager passed in; `server.bind` and `server.port` are not used"
                );
                let listener = Listener::from_inherited(socket)
                    .map_err(|e| CliError::runtime(e.to_string()))?;
                bind_listener(
                    &config,
                    listener,
                    tables,
                    sink.clone(),
                    Arc::new(SystemClock::new()),
                    Arc::new(OsRandom),
                )
            }
            None => bind_with(&config, tables, sink.clone()).await,
        }
        .map_err(|e| CliError::runtime(e.to_string()))?
        .with_usage(usage_sink, &history)
            .with_bodies(body_log(&config, &loaded.dirs.body_log(), err));
        let _ = writeln!(
            err,
            "airmate {} listening on {} ({}; {} upstream(s), {} model entr{})",
            env!("CARGO_PKG_VERSION"),
            gw.listen_url(),
            loaded.resolved.file.as_ref().map_or_else(
                || "built-in defaults".to_owned(),
                |f| format!("config {}", f.display())
            ),
            config.upstreams.len(),
            config.models.len(),
            if config.models.len() == 1 { "y" } else { "ies" },
        );
        let (tx, rx) = watch::channel(false);
        let shared = gw.shared();
        let serving = tokio::spawn(gw.serve(rx.clone()));
        let reload_ctx = ReloadCtx {
            p,
            global,
            flags: &flags_for_reload,
        };
        // Reloads asked for over the admin socket are carried out here, on the same code path as
        // SIGHUP, so both print the same messages and keep the old tables when a config is bad.
        let (reload_tx, mut reload_rx) = tokio::sync::mpsc::unbounded_channel::<
            tokio::sync::oneshot::Sender<Result<ReloadReport, String>>,
        >();
        let reload_fn: crate::admin::ReloadFn = Arc::new(move || {
            let (tx, rx) = tokio::sync::oneshot::channel();
            reload_tx
                .send(tx)
                .map_err(|_| "the gateway is shutting down".to_owned())?;
            rx.blocking_recv()
                .map_err(|_| "the gateway is shutting down".to_owned())?
        });
        let admin = start_admin(&loaded.dirs.admin_socket(), &shared, reload_fn, &rx, err);
        loop {
            tokio::select! {
                signal = signals.next() => match signal {
                    Signal::Reload => {
                        let _ = reload_now(&shared, &reload_ctx, err);
                    }
                    Signal::Stop => break,
                },
                Some(reply) = reload_rx.recv() => {
                    let _ = reply.send(reload_now(&shared, &reload_ctx, err));
                }
            }
        }
        let _ = writeln!(err, "airmate: shutting down");
        let _ = tx.send(true);
        let _ = serving.await;
        if let Some(admin) = admin {
            let _ = admin.await;
        }
        Ok(())
    })
}

struct ReloadCtx<'a> {
    p: &'a Process,
    global: &'a Global,
    flags: &'a Overrides,
}

/// Re-reads the configuration and swaps it in; a bad one is reported and the old one stays.
fn reload_now(
    shared: &crate::server::Shared,
    ctx: &ReloadCtx<'_>,
    err: &mut dyn Write,
) -> Result<ReloadReport, String> {
    let result = (|| -> Result<_, String> {
        let loaded = load(ctx.p, ctx.global, ctx.flags.clone()).map_err(|e| e.to_string())?;
        let token = gateway_token(ctx.p, &loaded).map_err(|e| e.to_string())?;
        let resolver = SystemResolver::new(&*ctx.p.env);
        reload(shared, loaded.resolved.config, &resolver, &token)
            .map_err(|errs| crate::config::ConfigErrors(errs).to_string())
    })();
    match &result {
        Ok(report) => {
            let _ = writeln!(err, "airmate: reloaded the configuration");
            for w in &report.warnings {
                let _ = writeln!(err, "airmate: warning: {w}");
            }
            for s in &report.restart_required {
                let _ = writeln!(
                    err,
                    "airmate: warning: {s} changed but only takes effect after a restart"
                );
            }
        }
        Err(e) => {
            let _ = writeln!(
                err,
                "airmate: reload rejected, the previous configuration stays in force:\n{e}"
            );
        }
    }
    result
}

/// Starts the admin interface. A gateway that cannot offer one still serves: the failure is
/// reported and the data plane carries on.
#[cfg(unix)]
fn start_admin(
    path: &std::path::Path,
    shared: &Arc<crate::server::Shared>,
    reload: crate::admin::ReloadFn,
    shutdown: &watch::Receiver<bool>,
    err: &mut dyn Write,
) -> Option<tokio::task::JoinHandle<()>> {
    match crate::admin::bind(path) {
        Ok(listener) => Some(tokio::spawn(crate::admin::serve(
            listener,
            path.to_owned(),
            shared.clone(),
            reload,
            shutdown.clone(),
        ))),
        Err(e) => {
            let _ = writeln!(
                err,
                "airmate: warning: no admin socket at {}: {e}; `airmate status` and `airmate reload` will not work",
                path.display()
            );
            None
        }
    }
}

#[cfg(not(unix))]
fn start_admin(
    _: &std::path::Path,
    _: &Arc<crate::server::Shared>,
    _: crate::admin::ReloadFn,
    _: &watch::Receiver<bool>,
    _: &mut dyn Write,
) -> Option<tokio::task::JoinHandle<()>> {
    None
}

/// What woke the server up.
enum Signal {
    Reload,
    Stop,
}

/// The signals the gateway reacts to.
struct Signals {
    #[cfg(unix)]
    term: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    int: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    hup: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    #[cfg(unix)]
    fn new() -> Self {
        use tokio::signal::unix::{signal, SignalKind};
        Self {
            term: signal(SignalKind::terminate()).ok(),
            int: signal(SignalKind::interrupt()).ok(),
            hup: signal(SignalKind::hangup()).ok(),
        }
    }

    #[cfg(not(unix))]
    fn new() -> Self {
        Self {}
    }

    #[cfg(unix)]
    async fn next(&mut self) -> Signal {
        async fn on(s: &mut Option<tokio::signal::unix::Signal>) {
            match s {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        }
        tokio::select! {
            () = on(&mut self.term) => Signal::Stop,
            () = on(&mut self.int) => Signal::Stop,
            () = on(&mut self.hup) => Signal::Reload,
        }
    }

    #[cfg(not(unix))]
    async fn next(&mut self) -> Signal {
        let _ = tokio::signal::ctrl_c().await;
        Signal::Stop
    }
}

/// Adds trace export to the request log's sink when the configuration asks for it. Header values
/// are secrets and are resolved here, so a missing one stops nothing but the export.
#[cfg(feature = "otel")]
fn with_otel(
    config: &crate::config::schema::Config,
    sink: Arc<dyn RecordSink>,
    resolver: &dyn crate::secret::SecretResolver,
    err: &mut dyn Write,
) -> Arc<dyn RecordSink> {
    let Some(o) = &config.otel else {
        return sink;
    };
    let mut headers = Vec::new();
    for (name, secret) in &o.headers {
        match resolver.resolve(secret) {
            Ok(r) => headers.push((name.clone(), r.secret.expose().to_owned())),
            Err(e) => {
                let _ = writeln!(
                    err,
                    "airmate: warning: otel header `{name}` is not sent: {e}"
                );
            }
        }
    }
    let _ = writeln!(err, "airmate: exporting traces to {}", o.endpoint);
    crate::otel::OtelSink::start(
        sink,
        &o.endpoint,
        &o.service_name,
        headers,
        std::time::Duration::from_secs(o.flush_secs),
    )
}

#[cfg(not(feature = "otel"))]
fn with_otel(
    config: &crate::config::schema::Config,
    sink: Arc<dyn RecordSink>,
    _: &dyn crate::secret::SecretResolver,
    err: &mut dyn Write,
) -> Arc<dyn RecordSink> {
    if config.otel.is_some() {
        let _ = writeln!(
            err,
            "airmate: warning: `otel` is set but this build has no `otel` feature; no traces are sent"
        );
    }
    sink
}

/// Opens the body log if the configuration asks for one. Bodies hold prompts and completions, so
/// a log that cannot be opened is reported and bodies are simply not written.
fn body_log(
    config: &crate::config::schema::Config,
    path: &std::path::Path,
    err: &mut dyn Write,
) -> Arc<dyn BodySink> {
    if !config.log.bodies {
        return Arc::new(NullBodies);
    }
    match JsonlBodies::open(
        path,
        config.log.body_max_bytes,
        config.log.body_retention_days,
    ) {
        Ok(log) => {
            let _ = writeln!(
                err,
                "airmate: warning: body logging is on: prompts and completions are written to {} (kept {} days)",
                path.display(),
                config.log.body_retention_days
            );
            Arc::new(log)
        }
        Err(e) => {
            let _ = writeln!(
                err,
                "airmate: warning: cannot open the body log {}: {e}; bodies are not logged",
                path.display()
            );
            Arc::new(NullBodies)
        }
    }
}

/// Opens the usage ledger if the configuration asks for one, and reads what is already in it so
/// that budgets survive a restart. Without a ledger, budgets still hold but start from zero.
fn usage_ledger(
    config: &crate::config::schema::Config,
    path: &std::path::Path,
    err: &mut dyn Write,
) -> (Arc<dyn UsageSink>, Vec<UsageRecord>) {
    let has_budget = config.clients.iter().any(|c| c.budget.is_some());
    if !config.usage.ledger {
        if has_budget {
            let _ = writeln!(
                err,
                "airmate: warning: clients have budgets but `usage.ledger` is off, so spend is forgotten on restart"
            );
        }
        return (Arc::new(NullUsage), Vec::new());
    }
    let history = match crate::usage::read_file(path) {
        Ok(h) => h,
        Err(e) => {
            let _ = writeln!(err, "airmate: warning: {e}");
            Vec::new()
        }
    };
    match JsonlLog::open(path, None) {
        Ok(log) => (Arc::new(log), history),
        Err(e) => {
            let _ = writeln!(
                err,
                "airmate: warning: cannot open the usage ledger {}: {e}",
                path.display()
            );
            (Arc::new(NullUsage), history)
        }
    }
}

/// Opens the request log if the configuration asks for one; a log that cannot be opened is
/// reported and skipped, since logging must never stop the gateway from serving.
fn request_log(
    config: &crate::config::schema::Config,
    path: &std::path::Path,
    err: &mut dyn Write,
) -> Arc<dyn RecordSink> {
    if !config.log.requests {
        return Arc::new(NullSink);
    }
    let rotation = config.log.rotate.map(|r| Rotation {
        max_bytes: r.max_size_mb * 1024 * 1024,
        keep: r.keep,
    });
    match JsonlLog::open(path, rotation) {
        Ok(log) => Arc::new(log),
        Err(e) => {
            let _ = writeln!(
                err,
                "airmate: warning: cannot open the request log {}: {e}",
                path.display()
            );
            Arc::new(NullSink)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Config;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("airmate-serve-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn text(buf: Vec<u8>) -> String {
        String::from_utf8(buf).unwrap()
    }

    /// A path whose parent is a file, so nothing can be opened there.
    fn unopenable(d: &std::path::Path) -> std::path::PathBuf {
        std::fs::write(d.join("file"), "x").unwrap();
        d.join("file/log.jsonl")
    }

    #[test]
    fn the_request_log_is_skipped_when_off_and_when_it_cannot_be_opened() {
        let d = dir("reqlog");
        let mut config = Config::default();
        config.log.requests = false;
        let mut err = Vec::new();
        let _ = request_log(&config, &d.join("requests.jsonl"), &mut err);
        assert!(err.is_empty());
        assert!(!d.join("requests.jsonl").exists());

        config.log.requests = true;
        let _ = request_log(&config, &unopenable(&d), &mut err);
        assert!(
            text(err).contains("cannot open the request log"),
            "an unwritable log warns and the gateway serves anyway"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_body_log_warns_that_it_is_on_and_when_it_cannot_be_opened() {
        let d = dir("bodies");
        let mut config = Config::default();
        let mut err = Vec::new();
        let _ = body_log(&config, &d.join("bodies.jsonl"), &mut err);
        assert!(err.is_empty(), "off by default, and silent");

        config.log.bodies = true;
        let _ = body_log(&config, &d.join("bodies.jsonl"), &mut err);
        assert!(
            text(std::mem::take(&mut err)).contains("body logging is on"),
            "prompts are being written to disk, which the operator is told"
        );
        let _ = body_log(&config, &unopenable(&d), &mut err);
        assert!(
            text(err).contains("bodies are not logged"),
            "an unopenable body log must not stop the gateway"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_usage_ledger_is_read_back_and_its_failures_are_reported() {
        let d = dir("ledger");
        let path = d.join("usage.jsonl");
        let mut config = Config::default();
        config.usage.ledger = false;
        let mut err = Vec::new();
        let (_, history) = usage_ledger(&config, &path, &mut err);
        assert!(history.is_empty() && err.is_empty());

        // Budgets without a ledger forget their spend on restart: say so.
        let with_budget = crate::config::load::parse(
            "clients:\n  - { name: ci, key_hash: \"sha256:0000000000000000000000000000000000000000000000000000000000000000\", budget: { usd_per_day: 5.0 } }\nusage: { ledger: false }\n",
            None,
        )
        .unwrap();
        let _ = usage_ledger(&with_budget, &path, &mut err);
        assert!(text(std::mem::take(&mut err)).contains("spend is forgotten on restart"),);

        config.usage.ledger = true;
        let (_, history) = usage_ledger(&config, &path, &mut err);
        assert!(
            history.is_empty() && err.is_empty(),
            "a new ledger starts empty"
        );
        assert!(path.exists());

        // A ledger that is a directory can be neither read nor appended to.
        let (_, history) = usage_ledger(&config, &d, &mut err);
        let t = text(err);
        assert!(history.is_empty());
        assert!(t.contains("cannot read the usage ledger"), "{t}");
        assert!(t.contains("cannot open the usage ledger"), "{t}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[cfg(feature = "otel")]
    #[tokio::test]
    async fn otel_export_reports_headers_it_cannot_resolve() {
        let config = crate::config::load::parse(
            "otel:\n  endpoint: http://127.0.0.1:9\n  headers:\n    x-ok: { env: PRESENT }\n    x-missing: { env: ABSENT }\n",
            None,
        )
        .unwrap();
        let env = |n: &str| (n == "PRESENT").then(|| "v".to_owned());
        let resolver = SystemResolver::new(&env);
        let mut err = Vec::new();
        let _ = with_otel(&Config::default(), Arc::new(NullSink), &resolver, &mut err);
        assert!(err.is_empty(), "no otel block, no export, no output");

        let _ = with_otel(&config, Arc::new(NullSink), &resolver, &mut err);
        let t = text(err);
        assert!(t.contains("exporting traces to http://127.0.0.1:9"), "{t}");
        assert!(t.contains("otel header `x-missing` is not sent"), "{t}");
        assert!(!t.contains("x-ok"), "{t}");
        assert_eq!(config.otel.as_ref().unwrap().service_name, "airmate");
        assert_eq!(config.otel.as_ref().unwrap().flush_secs, 5);
    }
}
