//! `airmate route` and `airmate models`: seeing what the router would do, without calling anyone.

use std::io::{Read, Write};
use std::sync::Arc;

use serde_json::json;

use airmate_core::adapters;
use airmate_core::facts::Facts;
use airmate_core::ir::Protocol;

use super::common::{load, CliError, Global, Process};
use crate::config::overrides::Overrides;
use crate::router::breaker::BreakerConfig;
use crate::router::catalog::Capabilities;
use crate::router::clock::{OsRandom, SystemClock};
use crate::router::engine::{plan, PlanInput, Route};
use crate::router::state::RouterState;
use crate::secret::{OfflineResolver, Secret};
use crate::server::Tables;

fn io(e: std::io::Error) -> CliError {
    CliError::runtime(format!("cannot write output: {e}"))
}

fn tables(p: &Process, global: &Global) -> Result<(Arc<Tables>, crate::config::Config), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let config = loaded.resolved.config.clone();
    let (tables, _) = Tables::build(
        Arc::new(config.clone()),
        &OfflineResolver,
        &Secret::new("offline"),
    )
    .map_err(|errs| CliError::usage(crate::config::ConfigErrors(errs).to_string()))?;
    Ok((Arc::new(tables), config))
}

/// What the router would do with a captured request.
pub struct Explained {
    /// The routing facts.
    pub facts: Facts,
    /// The planned route.
    pub route: Route,
}

fn read_request(path: &str) -> Result<Vec<u8>, CliError> {
    if path == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| CliError::runtime(format!("cannot read stdin: {e}")))?;
        Ok(buf)
    } else {
        std::fs::read(path).map_err(|e| CliError::usage(format!("cannot read {path}: {e}")))
    }
}

/// Plans a route for `body` as a client of `inbound` named `client`.
///
/// # Errors
///
/// Anything that would make the gateway refuse the request, with the gateway's own message.
pub async fn explain(
    tables: &Tables,
    config: &crate::config::Config,
    inbound: Protocol,
    client: &str,
    headers: &[(String, String)],
    body: &[u8],
    model: Option<&str>,
) -> Result<Explained, CliError> {
    let facts = if inbound == Protocol::Gemini {
        // A Gemini request names its model in the URL, not the body.
        let model = model.ok_or_else(|| {
            CliError::usage("a Gemini request has no model in its body: pass --model NAME")
        })?;
        adapters::peek_gemini(
            body,
            &airmate_core::inbound::gemini::Call {
                model,
                stream: false,
            },
        )
    } else {
        adapters::peek(inbound, body)
    }
    .map_err(|e| CliError::usage(e.message))?;
    let state = RouterState::new(
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom),
        BreakerConfig::from(&config.circuit),
        16,
    );
    let allowed: Vec<String> = config
        .server
        .rule_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    let hdrs: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    let header = move |name: &str| -> Option<String> {
        let n = name.to_ascii_lowercase();
        allowed
            .contains(&n)
            .then(|| hdrs.iter().find(|(k, _)| *k == n).map(|(_, v)| v.clone()))
            .flatten()
    };
    let route = plan(
        &tables.models,
        &tables.rules,
        &tables.catalog,
        &state,
        config,
        &PlanInput {
            facts: &facts,
            client,
            body,
            header: &header,
            force_entry: None,
            embeddings: false,
        },
    )
    .await
    .map_err(|e| CliError::usage(e.message))?;
    Ok(Explained { facts, route })
}

fn caps_line(c: &Capabilities) -> String {
    let mut f = Vec::new();
    for (on, name) in [
        (c.tools, "tools"),
        (c.vision, "vision"),
        (c.thinking, "thinking"),
        (c.effort, "effort"),
        (c.structured_output, "structured"),
    ] {
        if on {
            f.push(name);
        }
    }
    let ctx = c
        .context_window
        .map_or("unbounded".to_owned(), |w| format!("{w}"));
    let out = c
        .max_output_tokens
        .map_or("unknown".to_owned(), |w| format!("{w}"));
    format!("context {ctx}, max output {out}; {}", f.join(" "))
}

/// Renders an explanation as text.
#[must_use]
pub fn render_explanation(inbound: Protocol, x: &Explained, tables: &Tables) -> String {
    let (f, r) = (&x.facts, &x.route);
    let mut out = String::new();
    out.push_str(&format!(
        "request     {} model={} stream={} tools={} images={} thinking={} est_input_tokens={} max_tokens={}\n",
        inbound.as_str(),
        f.model,
        f.stream,
        f.tools,
        f.images,
        f.thinking,
        f.est_input_tokens,
        f.max_tokens
    ));
    match (&r.rule, &r.via) {
        (Some(rule), _) => out.push_str(&format!(
            "rule        `{rule}` matched, so it chose the entry `{}`\n",
            r.entry_name
        )),
        (None, Some(via)) => out.push_str(&format!(
            "rule        none matched; the name resolved to `{}` via {via}\n",
            r.entry_name
        )),
        (None, None) => out.push_str("rule        none matched\n"),
    }
    out.push_str(&format!("strategy    {:?}\n", r.strategy));
    if r.skipped.is_empty() {
        out.push_str("filtering   every target can serve the request\n");
    } else {
        for s in &r.skipped {
            out.push_str(&format!(
                "filtering   skipped {}: {}\n",
                s.target,
                s.reason.explain()
            ));
        }
    }
    for (i, p) in r.targets.iter().enumerate() {
        let caps = tables.catalog.for_target(&p.target);
        let blind = if caps.known {
            ""
        } else {
            "  [not in the catalog: the capability filter is blind for it]"
        };
        out.push_str(&format!(
            "order {:>2}    {}  ({}){blind}\n",
            i + 1,
            p.target.label(),
            caps_line(&caps)
        ));
    }
    match &r.affinity_key {
        Some(k) => out.push_str(&format!(
            "affinity    conversation {k}; a running gateway prefers the target that served its previous turn\n"
        )),
        None => out.push_str("affinity    off, or no conversation to key on\n"),
    }
    out
}

/// `airmate route [--explain] <request>`.
///
/// # Errors
///
/// The configuration or the request is invalid, or the router would refuse the request.
#[allow(clippy::too_many_arguments)]
pub fn route(
    p: &Process,
    global: &Global,
    inbound: Protocol,
    file: &str,
    client: &str,
    headers: &[String],
    explain_all: bool,
    json_out: bool,
    model: Option<&str>,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let (tables, config) = tables(p, global)?;
    let body = read_request(file)?;
    let hdrs: Vec<(String, String)> = headers
        .iter()
        .map(|h| {
            h.split_once('=')
                .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
                .ok_or_else(|| CliError::usage(format!("`{h}` is not NAME=VALUE")))
        })
        .collect::<Result<_, _>>()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::runtime(e.to_string()))?;
    let x = rt.block_on(explain(
        &tables, &config, inbound, client, &hdrs, &body, model,
    ))?;
    if json_out {
        let targets: Vec<_> = x.route.targets.iter().map(|t| t.target.label()).collect();
        let skipped: Vec<_> = x.route.skipped.iter().map(ToString::to_string).collect();
        let v = json!({"entry": x.route.entry_name, "rule": x.route.rule, "via": x.route.via.as_ref().map(ToString::to_string),
            "strategy": format!("{:?}", x.route.strategy), "targets": targets, "skipped": skipped});
        writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&v).expect("serialises")
        )
        .map_err(io)
    } else if explain_all {
        write!(out, "{}", render_explanation(inbound, &x, &tables)).map_err(io)
    } else {
        for t in &x.route.targets {
            writeln!(out, "{}", t.target.label()).map_err(io)?;
        }
        Ok(())
    }
}

/// `airmate models [--check]`.
///
/// # Errors
///
/// The configuration is invalid; with `check`, any target the catalog knows nothing about.
pub fn models(
    p: &Process,
    global: &Global,
    check: bool,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let (tables, _) = tables(p, global)?;
    let mut blind: Vec<String> = Vec::new();
    for e in tables.models.entries() {
        writeln!(
            out,
            "{}  (entry `{}`, strategy {:?})",
            e.pattern, e.name, e.strategy
        )
        .map_err(io)?;
        for (i, t) in e.targets.iter().enumerate() {
            let c = tables.catalog.for_target(t);
            let price = c.price.map_or("price unknown".to_owned(), |p| {
                format!("${}/{} per Mtok", p.input_per_mtok, p.output_per_mtok)
            });
            writeln!(
                out,
                "  {}. {}  [{}]  {}; {price}",
                i + 1,
                t.label(),
                t.upstream.protocol.as_str(),
                caps_line(&c)
            )
            .map_err(io)?;
            if !c.known {
                blind.push(t.label());
                writeln!(
                    out,
                    "     ! not in the catalog: the capability filter is blind for this model"
                )
                .map_err(io)?;
            }
        }
    }
    writeln!(out, "catalog as of {}", tables.catalog.as_of()).map_err(io)?;
    if check && !blind.is_empty() {
        return Err(CliError {
            code: super::common::EXIT_UNHEALTHY,
            message: format!("no catalog entry for: {}", blind.join(", ")),
        });
    }
    Ok(())
}
