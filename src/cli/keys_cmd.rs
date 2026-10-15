//! `airmate keys add|list|revoke|rotate`.
//!
//! A key is shown once, when it is created. Only its SHA-256 hash is stored, in the managed
//! `clients.yaml`; `--save` also keeps the key itself, owner-only, so `airmate env --client NAME`
//! and `airmate run --client NAME` can use it. A client defined in the operator's own config file
//! is listed but is revoked and rotated by editing that file.

use std::io::Write;
use std::path::Path;

use super::common::{load, CliError, Global, Process};
use crate::auth::{generate_client_key, hash_key};
use crate::config::clients_file::{self, ClientsFile};
use crate::config::overrides::Overrides;
use crate::config::schema::{Budget, ClientConfig, RateLimit};

fn io(e: std::io::Error) -> CliError {
    CliError::runtime(format!("cannot write output: {e}"))
}

/// What `keys add` sets besides the name.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Model globs the client may use; empty means all.
    pub models: Vec<String>,
    /// Requests per minute.
    pub rpm: Option<u32>,
    /// Input tokens per minute.
    pub tpm: Option<u64>,
    /// Spend cap per day, in US dollars.
    pub budget_per_day: Option<f64>,
    /// Spend cap per month, in US dollars.
    pub budget_per_month: Option<f64>,
    /// A model entry to fall back to when the budget is exhausted.
    pub budget_fallback: Option<String>,
    /// Also keep the key, owner-only, for `env --client` and `run --client`.
    pub save: bool,
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn save_key(path: &Path, key: &str) -> Result<(), CliError> {
    if let Some(dir) = path.parent() {
        let existed = dir.exists();
        std::fs::create_dir_all(dir)
            .map_err(|e| CliError::runtime(format!("cannot create {}: {e}", dir.display())))?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let _ = existed;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).map_err(|e| {
        CliError::runtime(format!("cannot save the key to {}: {e}", path.display()))
    })?;
    f.write_all(key.as_bytes())
        .map_err(|e| CliError::runtime(format!("cannot save the key: {e}")))
}

/// Tells a running gateway to pick the change up. Revocation takes effect on reload, so a
/// revoked key stops working as soon as this returns.
fn reload_hint(dirs: &crate::config::paths::Dirs, out: &mut dyn Write) -> Result<(), CliError> {
    let socket = dirs.admin_socket();
    if !socket.exists() {
        return writeln!(
            out,
            "No gateway is running; the change applies when one starts."
        )
        .map_err(io);
    }
    match super::admin_cmd::reload_running(&socket) {
        Ok(msg) => writeln!(out, "{msg}."),
        Err(e) => writeln!(
            out,
            "warning: the change is saved but the running gateway was not reloaded: {e}\nRun `airmate reload` once the configuration is fixed."
        ),
    }
    .map_err(io)
}

/// `keys add`.
///
/// # Errors
///
/// A bad name, a name already in use, or a file that cannot be written.
pub fn add(
    p: &Process,
    global: &Global,
    name: &str,
    o: &AddOptions,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    if !name_ok(name) {
        return Err(CliError::usage(format!(
            "`{name}` is not a client name: use letters, digits, `_`, `-` and `.`"
        )));
    }
    let loaded = load(p, global, Overrides::default())?;
    if loaded
        .resolved
        .config
        .clients
        .iter()
        .any(|c| c.name == name)
    {
        return Err(CliError::usage(format!(
            "a client named `{name}` already exists; use `keys rotate {name}` for a new key"
        )));
    }
    let key = generate_client_key().map_err(|e| CliError::runtime(e.to_string()))?;
    let budget = (o.budget_per_day.is_some() || o.budget_per_month.is_some()).then_some(Budget {
        usd_per_day: o.budget_per_day,
        usd_per_week: None,
        usd_per_month: o.budget_per_month,
    });
    let client = ClientConfig {
        name: name.to_owned(),
        key_hash: hash_key(&key),
        models: o.models.clone(),
        rate: (o.rpm.is_some() || o.tpm.is_some()).then_some(RateLimit {
            rpm: o.rpm,
            tpm: o.tpm,
        }),
        budget,
        budget_fallback: o.budget_fallback.clone(),
    };
    let mut file = clients_file::read(&loaded.dirs.clients_file())
        .map_err(|e| CliError::usage(e.to_string()))?;
    file.clients.push(client);
    // Validate the result as the gateway would, before writing anything.
    let mut check = loaded.resolved.config.clone();
    check
        .clients
        .extend(file.clients.iter().filter(|c| c.name == name).cloned());
    let problems = crate::config::validate(&check);
    if !problems.is_empty() {
        return Err(CliError::usage(
            crate::config::ConfigErrors(problems).to_string(),
        ));
    }
    clients_file::write(&loaded.dirs.clients_file(), &file)
        .map_err(|e| CliError::runtime(e.to_string()))?;
    if o.save {
        save_key(&loaded.dirs.client_key_file(name), &key)?;
    }
    writeln!(
        out,
        "client `{name}` created. Its key is shown once; store it now:\n\n{key}\n"
    )
    .map_err(io)?;
    if o.save {
        writeln!(
            out,
            "The key was also saved owner-only, so `airmate env --client {name}` can use it."
        )
        .map_err(io)?;
    }
    reload_hint(&loaded.dirs, out)
}

/// `keys list`.
///
/// # Errors
///
/// The configuration cannot be loaded.
pub fn list(p: &Process, global: &Global, out: &mut dyn Write) -> Result<(), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let managed = clients_file::read(&loaded.dirs.clients_file())
        .map_err(|e| CliError::usage(e.to_string()))?;
    if loaded.resolved.config.clients.is_empty() {
        return writeln!(
            out,
            "no named clients; every request uses the gateway token"
        )
        .map_err(io);
    }
    writeln!(
        out,
        "{:<16} {:<10} {:<24} {:<22} {:<20} saved key",
        "NAME", "SOURCE", "MODELS", "RATE", "BUDGET"
    )
    .map_err(io)?;
    for c in &loaded.resolved.config.clients {
        let source = if managed.clients.iter().any(|m| m.name == c.name) {
            "keys"
        } else {
            "config"
        };
        let models = if c.models.is_empty() {
            "all".to_owned()
        } else {
            c.models.join(",")
        };
        let rate = c.rate.map_or("-".to_owned(), |r| {
            [
                r.rpm.map(|n| format!("{n}/min")),
                r.tpm.map(|n| format!("{n} tok/min")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
        });
        let budget = c.budget.map_or("-".to_owned(), |b| {
            [
                b.usd_per_day.map(|n| format!("${n}/day")),
                b.usd_per_week.map(|n| format!("${n}/week")),
                b.usd_per_month.map(|n| format!("${n}/month")),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ")
        });
        let saved = if loaded.dirs.client_key_file(&c.name).exists() {
            "yes"
        } else {
            "no"
        };
        writeln!(
            out,
            "{:<16} {source:<10} {models:<24} {rate:<22} {budget:<20} {saved}",
            c.name
        )
        .map_err(io)?;
    }
    Ok(())
}

fn find_managed<'a>(
    file: &'a mut ClientsFile,
    loaded_clients: &[ClientConfig],
    name: &str,
) -> Result<&'a mut ClientConfig, CliError> {
    if let Some(i) = file.clients.iter().position(|c| c.name == name) {
        return Ok(&mut file.clients[i]);
    }
    if loaded_clients.iter().any(|c| c.name == name) {
        Err(CliError::usage(format!(
            "`{name}` is defined in the config file; edit it there"
        )))
    } else {
        Err(CliError::usage(format!(
            "no client named `{name}`; see `airmate keys list`"
        )))
    }
}

/// `keys revoke`.
///
/// # Errors
///
/// No such client, or one defined in the operator's own config file.
pub fn revoke(
    p: &Process,
    global: &Global,
    name: &str,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let mut file = clients_file::read(&loaded.dirs.clients_file())
        .map_err(|e| CliError::usage(e.to_string()))?;
    find_managed(&mut file, &loaded.resolved.config.clients, name)?;
    file.clients.retain(|c| c.name != name);
    clients_file::write(&loaded.dirs.clients_file(), &file)
        .map_err(|e| CliError::runtime(e.to_string()))?;
    let _ = std::fs::remove_file(loaded.dirs.client_key_file(name));
    writeln!(out, "client `{name}` revoked.").map_err(io)?;
    reload_hint(&loaded.dirs, out)
}

/// `keys rotate`: a new key, the same settings.
///
/// # Errors
///
/// No such client, or one defined in the operator's own config file.
pub fn rotate(
    p: &Process,
    global: &Global,
    name: &str,
    save: bool,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let mut file = clients_file::read(&loaded.dirs.clients_file())
        .map_err(|e| CliError::usage(e.to_string()))?;
    let key = generate_client_key().map_err(|e| CliError::runtime(e.to_string()))?;
    find_managed(&mut file, &loaded.resolved.config.clients, name)?.key_hash = hash_key(&key);
    clients_file::write(&loaded.dirs.clients_file(), &file)
        .map_err(|e| CliError::runtime(e.to_string()))?;
    let had_saved = loaded.dirs.client_key_file(name).exists();
    if save || had_saved {
        save_key(&loaded.dirs.client_key_file(name), &key)?;
    }
    writeln!(out, "client `{name}` has a new key. It is shown once; store it now:\n\n{key}\n\nThe old key stops working on the next reload.").map_err(io)?;
    reload_hint(&loaded.dirs, out)
}
