//! The managed client file.
//!
//! `airmate keys add|revoke|rotate` write client entries (a name, the SHA-256 hash of its key and
//! its limits) to `clients.yaml` next to the configuration file, so the operator's own file, and
//! the comments in it, are never rewritten. The two sources are merged when the configuration is
//! loaded; a name defined in both is an error. The file is `0600` in a `0700` directory, and a key
//! itself is never stored here: only its hash.

use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::schema::ClientConfig;
use super::validate::ConfigError;

/// The contents of `clients.yaml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct ClientsFile {
    /// The clients.
    pub clients: Vec<ClientConfig>,
}

/// Reads the managed file; a missing file is an empty one.
///
/// # Errors
///
/// The file exists but cannot be read or parsed.
pub fn read(path: &Path) -> Result<ClientsFile, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(ClientsFile::default()),
        Ok(text) => serde_norway::from_str(&text).map_err(|e| {
            let at = e.location().map_or(String::new(), |l| {
                format!(" (line {}, column {})", l.line(), l.column())
            });
            ConfigError::new(
                path.display().to_string(),
                format!("invalid client file: {e}{at}"),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ClientsFile::default()),
        Err(e) => Err(ConfigError::new(
            path.display().to_string(),
            format!("cannot read the client file: {e}"),
        )),
    }
}

/// Writes the managed file atomically, `0600`, creating its directory `0700`.
///
/// # Errors
///
/// The file cannot be written.
pub fn write(path: &Path, file: &ClientsFile) -> Result<(), ConfigError> {
    let fail = |m: String| ConfigError::new(path.display().to_string(), m);
    if let Some(dir) = path.parent() {
        let existed = dir.exists();
        std::fs::create_dir_all(dir)
            .map_err(|e| fail(format!("cannot create the directory: {e}")))?;
        #[cfg(unix)]
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        let _ = existed;
    }
    let text = format!(
        "# Managed by `airmate keys`. Keys are stored only as SHA-256 hashes.\n{}",
        serde_norway::to_string(file).map_err(|e| fail(e.to_string()))?
    );
    let tmp = path.with_extension("yaml.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(&tmp)
        .map_err(|e| fail(format!("cannot write: {e}")))?;
    f.write_all(text.as_bytes())
        .map_err(|e| fail(format!("cannot write: {e}")))?;
    f.sync_all()
        .map_err(|e| fail(format!("cannot write: {e}")))?;
    std::fs::rename(&tmp, path).map_err(|e| fail(format!("cannot replace the file: {e}")))
}

/// Merges the managed clients into `base`, refusing a name defined in both.
///
/// # Errors
///
/// A client name that appears in both places.
pub fn merge(base: &mut Vec<ClientConfig>, managed: &[ClientConfig]) -> Result<(), ConfigError> {
    for m in managed {
        if base.iter().any(|b| b.name == m.name) {
            return Err(ConfigError::new(
                format!("clients[{}]", m.name),
                "defined both in the config file and in clients.yaml; remove one",
            ));
        }
        base.push(m.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{Budget, RateLimit};

    fn client(name: &str) -> ClientConfig {
        ClientConfig {
            name: name.into(),
            key_hash: format!("sha256:{}", "ab".repeat(32)),
            models: vec!["claude-*".into()],
            rate: Some(RateLimit {
                rpm: Some(60),
                tpm: None,
            }),
            budget: Some(Budget {
                usd_per_day: Some(5.0),
                usd_per_week: None,
                usd_per_month: None,
            }),
            budget_fallback: None,
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("airmate-clients-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_missing_or_empty_file_is_no_clients() {
        let d = tmp("missing");
        assert_eq!(
            read(&d.join("clients.yaml")).unwrap(),
            ClientsFile::default()
        );
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("clients.yaml"), "  \n").unwrap();
        assert_eq!(
            read(&d.join("clients.yaml")).unwrap(),
            ClientsFile::default()
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn the_file_round_trips_and_is_private() {
        let d = tmp("roundtrip");
        let p = d.join("config").join("clients.yaml");
        let f = ClientsFile {
            clients: vec![client("ci"), client("laptop")],
        };
        write(&p, &f).unwrap();
        assert_eq!(read(&p).unwrap(), f);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# Managed by `airmate keys`"), "{text}");
        assert!(
            text.contains("key_hash") && !text.contains("am_"),
            "only hashes are stored"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(p.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert!(
            !d.join("config").join("clients.yaml.tmp").exists(),
            "the temporary file is gone"
        );
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_malformed_file_names_the_problem() {
        let d = tmp("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("clients.yaml"),
            "clients:\n  - name: x\n    nope: 1\n",
        )
        .unwrap();
        let e = read(&d.join("clients.yaml")).unwrap_err().to_string();
        assert!(e.contains("unknown field") && e.contains("line"), "{e}");
        std::fs::write(d.join("clients.yaml"), "clients: [").unwrap();
        assert!(read(&d.join("clients.yaml")).is_err());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn merging_refuses_a_duplicate_name() {
        let mut base = vec![client("ci")];
        merge(&mut base, &[client("laptop")]).unwrap();
        assert_eq!(base.len(), 2);
        let e = merge(&mut base, &[client("ci")]).unwrap_err().to_string();
        assert!(e.contains("clients[ci]") && e.contains("both"), "{e}");
    }
}
