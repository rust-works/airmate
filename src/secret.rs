//! Secrets.
//!
//! The configuration refers to a secret, never contains one. A [`Secret`] redacts itself in
//! `Debug` and `Display` and zeroes its memory on drop. The upstream key never reaches the client,
//! and the gateway token never reaches the upstream; neither is ever logged.

use std::fmt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::config::paths::EnvFn;
use crate::config::schema::SecretRef;

/// A secret string. Never printed; zeroed on drop.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Secret(String);

impl Secret {
    /// Wraps a value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Call sites should be few and obvious.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Why a secret could not be resolved. Messages never contain the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretError(pub String);

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SecretError {}

fn err(message: impl Into<String>) -> SecretError {
    SecretError(message.into())
}

/// Resolves [`SecretRef`]s to [`Secret`]s. An extension trait of the library API.
pub trait SecretResolver: Send + Sync {
    /// Resolves a reference.
    ///
    /// # Errors
    ///
    /// The source is missing, unreadable, too permissive, or empty.
    fn resolve(&self, r: &SecretRef) -> Result<Resolved, SecretError>;
}

/// A resolved secret, plus a warning to show the operator if the source is discouraged.
#[derive(Debug)]
pub struct Resolved {
    /// The secret.
    pub secret: Secret,
    /// A warning to log, if any.
    pub warning: Option<String>,
}

/// The default resolver: environment, files, credential helpers and the OS keychain.
pub struct SystemResolver<'a> {
    env: EnvFn<'a>,
    command_timeout: Duration,
}

impl<'a> SystemResolver<'a> {
    /// A resolver reading variables through `env`.
    #[must_use]
    pub fn new(env: EnvFn<'a>) -> Self {
        Self {
            env,
            command_timeout: Duration::from_secs(10),
        }
    }

    /// Sets how long a credential helper may run.
    #[must_use]
    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }
}

fn nonempty(secret: String, source: &str) -> Result<Secret, SecretError> {
    let trimmed = secret.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        Err(err(format!("{source} is empty")))
    } else {
        Ok(Secret::new(trimmed))
    }
}

/// Refuses a secret file that group or others can read.
///
/// # Errors
///
/// The file is missing, not a regular file, or too permissive.
pub fn check_owner_only(path: &Path) -> Result<(), SecretError> {
    let meta =
        std::fs::metadata(path).map_err(|e| err(format!("cannot read {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(err(format!("{} is not a regular file", path.display())));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(err(format!(
                "{} is readable by group or others (mode {:o}); run `chmod 600 {}`",
                path.display(),
                mode & 0o777,
                path.display()
            )));
        }
    }
    Ok(())
}

fn run_helper(argv: &[String], timeout: Duration) -> Result<String, SecretError> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| err("the credential helper command is empty"))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| err(format!("cannot run the credential helper `{program}`: {e}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| err("no output from the credential helper"))?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut stdout, &mut buf).map(|_| buf)
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(err(format!(
                    "the credential helper `{program}` did not finish within {}s",
                    timeout.as_secs()
                )));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                return Err(err(format!(
                    "cannot wait for the credential helper `{program}`: {e}"
                )))
            }
        }
    };
    if !status.success() {
        return Err(err(format!(
            "the credential helper `{program}` failed ({status})"
        )));
    }
    reader
        .join()
        .map_err(|_| err("the credential helper output could not be read"))?
        .map_err(|e| err(format!("cannot read the credential helper output: {e}")))
}

impl SecretResolver for SystemResolver<'_> {
    fn resolve(&self, r: &SecretRef) -> Result<Resolved, SecretError> {
        let mut warning = None;
        let secret = match r {
            SecretRef::Env(name) => nonempty(
                (self.env)(name).unwrap_or_default(),
                &format!("the environment variable {name}"),
            )?,
            SecretRef::File(path) => {
                let p = Path::new(path);
                check_owner_only(p)?;
                nonempty(
                    std::fs::read_to_string(p)
                        .map_err(|e| err(format!("cannot read {path}: {e}")))?,
                    &format!("the file {path}"),
                )?
            }
            SecretRef::Command(argv) => nonempty(
                run_helper(argv, self.command_timeout)?,
                "the credential helper output",
            )?,
            SecretRef::Keychain(service) => crate::keychain::lookup(service)?,
            SecretRef::Value(v) => {
                warning = Some(
                    "a literal secret is set in the configuration file; use env, file or command instead"
                        .to_owned(),
                );
                nonempty(v.0.clone(), "the literal secret")?
            }
        };
        Ok(Resolved { secret, warning })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Redacted;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("airmate-secret-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_secret_never_prints() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(format!("{s}"), "<redacted>");
        assert_eq!(s.expose(), "hunter2");
        assert!(!format!(
            "{:?}",
            Resolved {
                secret: s,
                warning: None
            }
        )
        .contains("hunter2"));
    }

    #[test]
    fn env_secrets_resolve_and_missing_or_empty_ones_fail() {
        let e = env(&[("K", "v\n"), ("EMPTY", "")]);
        let r = SystemResolver::new(&e);
        assert_eq!(
            r.resolve(&SecretRef::Env("K".into()))
                .unwrap()
                .secret
                .expose(),
            "v"
        );
        let missing = r
            .resolve(&SecretRef::Env("NOPE".into()))
            .unwrap_err()
            .to_string();
        assert!(
            missing.contains("NOPE") && missing.contains("empty"),
            "{missing}"
        );
        assert!(r.resolve(&SecretRef::Env("EMPTY".into())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_secrets_must_be_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("file");
        let p = d.join("key");
        std::fs::write(&p, "from-file\n").unwrap();
        let e = env(&[]);
        let r = SystemResolver::new(&e);
        let reff = SecretRef::File(p.display().to_string());
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let denied = r.resolve(&reff).unwrap_err().to_string();
        assert!(
            denied.contains("readable by group or others") && denied.contains("chmod 600"),
            "{denied}"
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(r.resolve(&reff).unwrap().secret.expose(), "from-file");
        std::fs::write(&p, "\n").unwrap();
        assert!(r.resolve(&reff).unwrap_err().to_string().contains("empty"));
        assert!(r
            .resolve(&SecretRef::File(d.join("missing").display().to_string()))
            .is_err());
        assert!(r
            .resolve(&SecretRef::File(d.display().to_string()))
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn credential_helpers_run_without_a_shell_and_stdout_is_the_secret() {
        let e = env(&[]);
        let r = SystemResolver::new(&e);
        let ok = r
            .resolve(&SecretRef::Command(vec![
                "printf".into(),
                "tok-%s".into(),
                "123".into(),
            ]))
            .unwrap();
        assert_eq!(ok.secret.expose(), "tok-123");
        // No shell: metacharacters are literal arguments.
        let literal = r
            .resolve(&SecretRef::Command(vec!["echo".into(), "a;echo b".into()]))
            .unwrap();
        assert_eq!(literal.secret.expose(), "a;echo b");
        assert!(r
            .resolve(&SecretRef::Command(vec!["false".into()]))
            .unwrap_err()
            .to_string()
            .contains("failed"));
        assert!(r
            .resolve(&SecretRef::Command(vec!["airmate-no-such-helper".into()]))
            .unwrap_err()
            .to_string()
            .contains("cannot run"));
        assert!(r.resolve(&SecretRef::Command(vec![])).is_err());
        assert!(r
            .resolve(&SecretRef::Command(vec!["true".into()]))
            .unwrap_err()
            .to_string()
            .contains("empty"));
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_credential_helper_is_killed_at_the_timeout() {
        let e = env(&[]);
        let r = SystemResolver::new(&e).with_command_timeout(Duration::from_millis(100));
        let started = Instant::now();
        let err = r
            .resolve(&SecretRef::Command(vec!["sleep".into(), "30".into()]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("did not finish"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_literal_resolves_with_a_loud_warning() {
        let e = env(&[]);
        let r = SystemResolver::new(&e)
            .resolve(&SecretRef::Value(Redacted("lit".into())))
            .unwrap();
        assert_eq!(r.secret.expose(), "lit");
        assert!(r.warning.unwrap().contains("literal secret"));
        assert!(SystemResolver::new(&e)
            .resolve(&SecretRef::Value(Redacted(String::new())))
            .is_err());
    }

    #[test]
    fn the_keychain_reports_when_it_is_unavailable() {
        let e = env(&[]);
        let result = SystemResolver::new(&e)
            .resolve(&SecretRef::Keychain("airmate-test-no-such-service".into()));
        assert!(result.is_err());
    }
}
