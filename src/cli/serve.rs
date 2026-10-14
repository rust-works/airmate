//! `airmate serve`: the gateway in the foreground.

use std::io::Write;
use std::sync::Arc;

use tokio::sync::watch;

use super::common::{gateway_token, load, CliError, Global, Process};
use crate::config::overrides::Overrides;
use crate::obs::{JsonlLog, NullSink, RecordSink, Rotation};
use crate::secret::SystemResolver;
use crate::server::reload::{reload, ReloadReport};
use crate::server::{bind_with, Tables};
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
        let (usage_sink, history) = usage_ledger(&config, &loaded.dirs.usage_ledger(), err);
        let gw = bind_with(&config, tables, sink.clone())
            .await
            .map_err(|e| CliError::runtime(e.to_string()))?
            .with_usage(usage_sink, &history);
        let _ = writeln!(
            err,
            "airmate {} listening on http://{} ({}; {} upstream(s), {} model entr{})",
            env!("CARGO_PKG_VERSION"),
            gw.local_addr(),
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
        let mut signals = Signals::new();
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
