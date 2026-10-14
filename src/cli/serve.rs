//! `airmate serve`: the gateway in the foreground.

use std::io::Write;
use std::sync::Arc;

use tokio::sync::watch;

use super::common::{gateway_token, load, CliError, Global, Process};
use crate::config::overrides::Overrides;
use crate::obs::{JsonlLog, NullSink, RecordSink, Rotation};
use crate::secret::SystemResolver;
use crate::server::reload::reload;
use crate::server::{bind_with, Tables};

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
        let gw = bind_with(&config, tables, sink.clone())
            .await
            .map_err(|e| CliError::runtime(e.to_string()))?;
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
        let serving = tokio::spawn(gw.serve(rx));
        let reload_ctx = ReloadCtx {
            p,
            global,
            flags: &flags_for_reload,
        };
        while let Signal::Reload = wait_for_signal().await {
            reload_now(&shared, &reload_ctx, err);
        }
        let _ = writeln!(err, "airmate: shutting down");
        let _ = tx.send(true);
        let _ = serving.await;
        Ok(())
    })
}

struct ReloadCtx<'a> {
    p: &'a Process,
    global: &'a Global,
    flags: &'a Overrides,
}

/// Re-reads the configuration and swaps it in; a bad one is reported and the old one stays.
fn reload_now(shared: &crate::server::Shared, ctx: &ReloadCtx<'_>, err: &mut dyn Write) {
    let result = (|| -> Result<_, String> {
        let loaded = load(ctx.p, ctx.global, ctx.flags.clone()).map_err(|e| e.to_string())?;
        let token = gateway_token(ctx.p, &loaded).map_err(|e| e.to_string())?;
        let resolver = SystemResolver::new(&*ctx.p.env);
        reload(shared, loaded.resolved.config, &resolver, &token)
            .map_err(|errs| crate::config::ConfigErrors(errs).to_string())
    })();
    match result {
        Ok(report) => {
            let _ = writeln!(err, "airmate: reloaded the configuration");
            for w in report.warnings {
                let _ = writeln!(err, "airmate: warning: {w}");
            }
            for s in report.restart_required {
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
}

/// What woke the server up.
enum Signal {
    Reload,
    Stop,
}

#[cfg(unix)]
async fn wait_for_signal() -> Signal {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int), Ok(mut hup)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
        signal(SignalKind::hangup()),
    ) else {
        std::future::pending::<()>().await;
        return Signal::Stop;
    };
    tokio::select! {
        _ = term.recv() => Signal::Stop,
        _ = int.recv() => Signal::Stop,
        _ = hup.recv() => Signal::Reload,
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() -> Signal {
    let _ = tokio::signal::ctrl_c().await;
    Signal::Stop
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
