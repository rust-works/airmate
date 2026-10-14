//! `airmate doctor`: a live self-test of each configured upstream.

use std::io::Write;
use std::sync::Arc;

use super::common::{load, CliError, Global, Process, EXIT_UNHEALTHY};
use crate::config::overrides::Overrides;
use crate::doctor::{render, run_target, Report};
use crate::secret::{Secret, SystemResolver};
use crate::server::Tables;

fn io(e: std::io::Error) -> CliError {
    CliError::runtime(format!("cannot write output: {e}"))
}

/// Runs the probes against the upstreams (or the one named) and prints the results. Exit 3 if
/// any probe fails.
///
/// # Errors
///
/// A configuration problem, an unknown upstream, or a failed probe (exit 3).
pub fn doctor(
    p: &Process,
    global: &Global,
    upstream: Option<&str>,
    model: Option<&str>,
    json: bool,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let config = Arc::new(loaded.resolved.config.clone());
    let resolver = SystemResolver::new(&*p.env);
    let (tables, _) = Tables::build(config.clone(), &resolver, &Secret::new("doctor"))
        .map_err(|errs| CliError::usage(crate::config::ConfigErrors(errs).to_string()))?;
    if let Some(name) = upstream {
        if tables.models.upstream(name).is_none() {
            return Err(CliError::usage(format!(
                "`{name}` is not a configured upstream"
            )));
        }
    }

    // One target per upstream: the one named by --model, else the first model entry that uses it.
    let mut targets = Vec::new();
    let mut names: Vec<&str> = config.upstreams.iter().map(|u| u.name.as_str()).collect();
    names.sort_unstable();
    for name in names
        .into_iter()
        .filter(|n| upstream.is_none_or(|u| u == *n))
    {
        let found = tables
            .models
            .entries()
            .iter()
            .flat_map(|e| e.targets.iter())
            .find(|t| t.upstream.name == name && model.is_none_or(|m| t.model == m));
        match found {
            Some(t) => targets.push(t.clone()),
            None => {
                let _ = writeln!(
                    std::io::stderr(),
                    "airmate: no model entry uses the upstream `{name}`{}; skipping it",
                    model.map_or(String::new(), |m| format!(" with the model `{m}`"))
                );
            }
        }
    }
    if targets.is_empty() {
        return Err(CliError::usage(
            "there is nothing to test: no upstream has a model entry",
        ));
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::runtime(format!("cannot start the async runtime: {e}")))?;
    let reports: Vec<Report> = rt.block_on(async {
        let mut r = Vec::new();
        for t in &targets {
            let caps = tables.catalog.for_target(t);
            r.push(run_target(t, &caps).await);
        }
        r
    });
    if json {
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&reports).unwrap_or_default()
        )
        .map_err(io)?;
    } else {
        write!(out, "{}", render(&reports)).map_err(io)?;
    }
    if reports.iter().any(Report::failed) {
        let mut e = CliError::runtime("one or more probes failed");
        e.code = EXIT_UNHEALTHY;
        return Err(e);
    }
    Ok(())
}
