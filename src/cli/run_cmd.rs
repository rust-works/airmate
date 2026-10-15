//! `airmate run -- <command>`: a command with its client settings pointed at the gateway.

use std::io::Write;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use super::common::{gateway_token, load, CliError, Global, Process};
use super::env_cmd::{client_host, variables, Family};
use crate::config::overrides::Overrides;
use crate::config::paths::Dirs;
use crate::secret::{Secret, SystemResolver};
use crate::server::{bind, Tables};

/// A named client's saved key: `AIRMATE_CLIENT_KEY_<NAME>`, else `<state>/client-keys/<NAME>`.
///
/// # Errors
///
/// No key is saved for the client.
pub fn client_key(p: &Process, dirs: &Dirs, name: &str) -> Result<Secret, CliError> {
    let var = format!(
        "AIRMATE_CLIENT_KEY_{}",
        name.to_ascii_uppercase().replace(['-', '.'], "_")
    );
    if let Some(v) = p.var(&var) {
        return Ok(Secret::new(v));
    }
    let path = dirs.state.join("client-keys").join(name);
    crate::secret::check_owner_only(&path).map_err(|e| {
        CliError::usage(format!(
            "no saved key for client `{name}` ({e}); set {var}, or create it with `airmate keys add {name} --save`"
        ))
    })?;
    std::fs::read_to_string(&path)
        .map(|s| Secret::new(s.trim()))
        .map_err(|e| CliError::runtime(format!("cannot read {}: {e}", path.display())))
}

fn probe(host: &str, port: u16) -> Option<SocketAddr> {
    let addr = (host.trim_matches(['[', ']']), port)
        .to_socket_addrs()
        .ok()?
        .next()?;
    TcpStream::connect_timeout(&addr, Duration::from_millis(300))
        .ok()
        .map(|_| addr)
}

/// Runs `command` with the client variables set, starting a private gateway if none is running.
///
/// # Errors
///
/// The configuration is invalid, the gateway cannot start, or the command cannot be run.
pub fn run(
    p: &Process,
    global: &Global,
    client: Option<&str>,
    command: &[String],
    err: &mut dyn Write,
) -> Result<(), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let token = match client {
        Some(name) => client_key(p, &loaded.dirs, name)?,
        None => gateway_token(p, &loaded)?,
    };
    let config = &loaded.resolved.config;
    let host = client_host(&config.server.bind);
    let running = (config.server.port != 0)
        .then(|| probe(&host, config.server.port))
        .flatten();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::runtime(format!("cannot start the async runtime: {e}")))?;
    let code = rt.block_on(async {
        let mut stop: Option<(watch::Sender<bool>, tokio::task::JoinHandle<()>)> = None;
        let base = if let Some(addr) = running {
            let _ = writeln!(err, "airmate: using the gateway already running at {addr}");
            format!("http://{host}:{}", config.server.port)
        } else {
            let mut private = config.clone();
            private.server.port = 0;
            let gateway_secret = gateway_token(p, &loaded)?;
            let resolver = SystemResolver::new(&*p.env);
            let (tables, warnings) =
                Tables::build(Arc::new(private.clone()), &resolver, &gateway_secret).map_err(
                    |errs| CliError::usage(crate::config::ConfigErrors(errs).to_string()),
                )?;
            for w in &warnings {
                let _ = writeln!(err, "airmate: warning: {w}");
            }
            let gw = bind(&private, tables)
                .await
                .map_err(|e| CliError::runtime(e.to_string()))?;
            let addr = gw.local_addr();
            let _ = writeln!(err, "airmate: started a private gateway on http://{addr}");
            let (tx, rx) = watch::channel(false);
            stop = Some((tx, tokio::spawn(gw.serve(rx))));
            format!(
                "http://{}:{}",
                client_host(&private.server.bind),
                addr.port()
            )
        };
        let mut env = variables(Family::Anthropic, &base, token.expose());
        env.extend(variables(Family::OpenAi, &base, token.expose()));
        let (program, args) = command
            .split_first()
            .ok_or_else(|| CliError::usage("no command given"))?;
        let status = tokio::task::spawn_blocking({
            let (program, args) = (program.clone(), args.to_vec());
            move || {
                std::process::Command::new(&program)
                    .args(&args)
                    .envs(&env)
                    .status()
                    .map_err(|e| (program, e))
            }
        })
        .await
        .map_err(|e| CliError::runtime(e.to_string()))?
        .map_err(|(program, e)| CliError::runtime(format!("cannot run `{program}`: {e}")))?;
        if let Some((tx, handle)) = stop {
            let _ = tx.send(true);
            let _ = handle.await;
        }
        Ok::<_, CliError>(status.code().map_or(1, |c| u8::try_from(c).unwrap_or(1)))
    })?;
    if code == 0 {
        Ok(())
    } else {
        Err(CliError {
            code,
            message: format!("`{}` exited with status {code}", command[0]),
        })
    }
}
