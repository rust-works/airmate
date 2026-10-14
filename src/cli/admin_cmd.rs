//! `airmate status` and `airmate reload`: talking to a running gateway over its admin socket.

use std::io::Write;
use std::path::Path;

use serde_json::Value;

use super::common::{CliError, Process, EXIT_UNHEALTHY};

fn io(e: std::io::Error) -> CliError {
    CliError::runtime(format!("cannot write output: {e}"))
}

/// How `status` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// For people.
    Text,
    /// The raw JSON.
    Json,
}

/// Sends a request to the admin socket.
///
/// # Errors
///
/// The gateway is not running, or the exchange failed.
#[cfg(unix)]
pub fn call(socket: &Path, method: &str, uri: &str) -> Result<(u16, String), CliError> {
    if !socket.exists() {
        return Err(CliError::runtime(format!(
            "airmate is not running (no admin socket at {})",
            socket.display()
        )));
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| CliError::runtime(format!("cannot start the async runtime: {e}")))?;
    rt.block_on(crate::admin::call(socket, method, uri))
        .map_err(|e| CliError::runtime(format!("airmate is not answering: {e}")))
}

/// Sends a request to the admin socket.
///
/// # Errors
///
/// Always: there is no admin interface on this platform yet.
#[cfg(not(unix))]
pub fn call(_: &Path, _: &str, _: &str) -> Result<(u16, String), CliError> {
    Err(CliError::runtime(
        "the admin interface needs Unix-domain sockets, which this platform does not offer yet",
    ))
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("-").to_owned()
}

fn render(v: &Value) -> String {
    let mut s = String::new();
    let secs = v["uptime_secs"].as_u64().unwrap_or(0);
    s.push_str(&format!(
        "airmate {} (pid {}) listening on {}, up {}h {}m {}s\nconfig {}, {} request(s) in flight\n",
        text(v, "version"),
        v["pid"],
        text(v, "listen"),
        secs / 3600,
        secs % 3600 / 60,
        secs % 60,
        text(v, "config_hash"),
        v["requests_in_flight"],
    ));
    if v["upstream_key_rejections"].as_u64().unwrap_or(0) > 0 {
        s.push_str(&format!(
            "WARNING: upstreams have rejected their API key {} time(s)\n",
            v["upstream_key_rejections"]
        ));
    }
    for u in v["upstreams"].as_array().into_iter().flatten() {
        let healthy = u["healthy"].as_bool().unwrap_or(true);
        s.push_str(&format!(
            "\nupstream {} ({}, {}): {}{}\n",
            text(u, "name"),
            text(u, "protocol"),
            text(u, "url"),
            if healthy { "healthy" } else { "UNHEALTHY" },
            u["health_error"]
                .as_str()
                .map_or(String::new(), |e| format!(": {e}")),
        ));
        for t in u["targets"].as_array().into_iter().flatten() {
            s.push_str(&format!(
                "  {}  circuit {}  in flight {}{}{}\n",
                text(t, "target"),
                text(t, "circuit"),
                t["inflight"],
                t["ttfb_ms"]
                    .as_f64()
                    .map_or(String::new(), |m| format!("  ttfb {m:.0}ms")),
                t["last_error"]
                    .as_str()
                    .map_or(String::new(), |e| format!("  last error: {e}")),
            ));
        }
    }
    if let Some(m) = v["ignored_fields"].as_object().filter(|m| !m.is_empty()) {
        s.push_str("\nfields clients sent that were ignored:\n");
        for (k, n) in m {
            s.push_str(&format!("  {k}  x{n}\n"));
        }
    }
    let errors = v["recent_errors"].as_array().cloned().unwrap_or_default();
    if !errors.is_empty() {
        s.push_str("\nrecent errors:\n");
        for e in errors.iter().take(5) {
            s.push_str(&format!(
                "  {} {} client {} {}{}\n",
                text(e, "ts"),
                e["status"],
                text(e, "client"),
                e["error_class"].as_str().unwrap_or(""),
                e["target"]
                    .as_str()
                    .map_or(String::new(), |t| format!(" via {t}")),
            ));
        }
    }
    s
}

/// Prints the state of the running gateway. Exit 3 if it is not running or is unhealthy.
///
/// # Errors
///
/// Exit 3 for a gateway that is not running or not healthy.
pub fn status(p: &Process, format: Format, out: &mut dyn Write) -> Result<(), CliError> {
    // The socket's location does not depend on the configuration, so a broken config file does
    // not stop an operator from looking at the gateway that is running.
    let socket = p.dirs().admin_socket();
    let (code, body) = match call(&socket, "GET", "/status") {
        Ok(r) => r,
        Err(mut e) => {
            e.code = EXIT_UNHEALTHY;
            return Err(e);
        }
    };
    if code != 200 {
        return Err(CliError::runtime(format!(
            "the admin interface answered {code}: {body}"
        )));
    }
    let v: Value = serde_json::from_str(&body)
        .map_err(|e| CliError::runtime(format!("unreadable status: {e}")))?;
    match format {
        Format::Json => {
            writeln!(out, "{}", serde_json::to_string_pretty(&v).unwrap_or(body)).map_err(io)?
        }
        Format::Text => write!(out, "{}", render(&v)).map_err(io)?,
    }
    if v["ok"].as_bool().unwrap_or(false) {
        Ok(())
    } else {
        let mut e = CliError::runtime(
            "unhealthy: an upstream is failing its health check or a circuit is open",
        );
        e.code = EXIT_UNHEALTHY;
        Err(e)
    }
}

/// Asks the running gateway to re-read its configuration.
///
/// # Errors
///
/// The gateway is not running, or it rejected the new configuration (which stays unapplied).
pub fn reload(p: &Process, out: &mut dyn Write) -> Result<(), CliError> {
    let msg = reload_running(&p.dirs().admin_socket())?;
    writeln!(out, "{msg}").map_err(io)
}

/// Reloads the gateway behind `socket`; returns what to tell the operator.
///
/// # Errors
///
/// The gateway is not running, or it rejected the configuration.
pub fn reload_running(socket: &Path) -> Result<String, CliError> {
    let (code, body) = call(socket, "POST", "/reload")?;
    let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    if code == 200 {
        let mut msg = "reloaded the running gateway".to_owned();
        for w in v["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            msg.push_str(&format!("\nwarning: {w}"));
        }
        for r in v["restart_required"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            msg.push_str(&format!(
                "\nwarning: {r} changed but only takes effect after a restart"
            ));
        }
        Ok(msg)
    } else {
        Err(CliError::usage(format!(
            "the gateway rejected the configuration and keeps the previous one:\n{}",
            v["error"].as_str().unwrap_or(&body)
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_text_view_names_upstreams_failures_and_dropped_fields() {
        let v = json!({
            "ok": false, "version": "1.2.3", "pid": 42, "uptime_secs": 3725, "listen": "127.0.0.1:9997",
            "config_hash": "abc", "requests_in_flight": 2, "upstream_key_rejections": 1,
            "upstreams": [{"name": "fw", "protocol": "chat", "url": "https://x", "healthy": false, "health_error": "503",
                "targets": [{"target": "fw/m", "circuit": "open", "inflight": 0, "ttfb_ms": 120.4, "last_error": "boom"}]}],
            "ignored_fields": {"top_k": 7},
            "recent_errors": [{"ts": "T", "status": 502, "client": "ci", "error_class": "api_error", "target": "fw/m"}],
        });
        let t = render(&v);
        for want in [
            "airmate 1.2.3 (pid 42)",
            "up 1h 2m 5s",
            "rejected their API key 1 time(s)",
            "upstream fw (chat, https://x): UNHEALTHY: 503",
            "fw/m  circuit open",
            "ttfb 120ms",
            "last error: boom",
            "top_k  x7",
            "502 client ci api_error via fw/m",
        ] {
            assert!(t.contains(want), "missing {want:?} in\n{t}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_missing_socket_means_not_running() {
        let e = call(Path::new("/definitely/not/there.sock"), "GET", "/status").unwrap_err();
        assert!(e.message.contains("not running"), "{}", e.message);
    }
}
