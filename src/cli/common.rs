//! What every command needs: the process environment, error type, and config loading.

use std::fmt;
use std::path::PathBuf;

use crate::config::overrides::Overrides;
use crate::config::paths::{config_file, Dirs, Platform};
use crate::config::resolve::{read_config_file, resolve, Inputs, Resolved};
use crate::config::ConfigErrors;
use crate::secret::{Secret, SecretResolver, SystemResolver};

/// The exit codes: 0 ok, 1 runtime error, 2 usage or config error, 3 unhealthy.
pub const EXIT_RUNTIME: u8 = 1;
/// Usage or configuration error.
pub const EXIT_USAGE: u8 = 2;
/// A health check failed.
pub const EXIT_UNHEALTHY: u8 = 3;

/// A failure with the exit code it maps to.
#[derive(Debug)]
pub struct CliError {
    /// The exit code.
    pub code: u8,
    /// What to tell the operator.
    pub message: String,
}

impl CliError {
    /// A runtime failure (exit 1).
    #[must_use]
    pub fn runtime(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_RUNTIME,
            message: message.into(),
        }
    }

    /// A usage or configuration failure (exit 2).
    #[must_use]
    pub fn usage(message: impl Into<String>) -> Self {
        Self {
            code: EXIT_USAGE,
            message: message.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliError {}

impl From<ConfigErrors> for CliError {
    fn from(e: ConfigErrors) -> Self {
        Self::usage(e.to_string())
    }
}

/// Looks up an environment variable.
pub type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The process environment, injectable for tests.
pub struct Process {
    /// Environment variable lookup.
    pub env: EnvLookup,
    /// Whose path conventions apply.
    pub platform: Platform,
}

impl Process {
    /// The real process.
    #[must_use]
    pub fn real() -> Self {
        Self {
            env: Box::new(|k| std::env::var(k).ok()),
            platform: Platform::current(),
        }
    }

    /// The directories in use.
    #[must_use]
    pub fn dirs(&self) -> Dirs {
        Dirs::resolve(self.platform, &*self.env)
    }

    /// Looks a variable up.
    #[must_use]
    pub fn var(&self, name: &str) -> Option<String> {
        (self.env)(name).filter(|v| !v.is_empty())
    }
}

/// The settings every command can take.
#[derive(Debug, Clone, Default)]
pub struct Global {
    /// `--config`.
    pub config: Option<PathBuf>,
    /// `--profile`.
    pub profile: Option<String>,
}

/// A loaded configuration with what was read to produce it.
pub struct Loaded {
    /// The effective configuration.
    pub resolved: Resolved,
    /// The directories in use.
    pub dirs: Dirs,
    /// The config file text, if one was read.
    pub file_text: Option<String>,
    /// The path the config file was looked for at.
    pub file_path: PathBuf,
}

/// Loads and resolves the configuration.
///
/// # Errors
///
/// A usage error for anything wrong with the configuration.
pub fn load(p: &Process, global: &Global, flags: Overrides) -> Result<Loaded, CliError> {
    let dirs = p.dirs();
    let env = Overrides::from_env(&*p.env)?;
    let explicit = global.config.is_some() || p.var("AIRMATE_CONFIG").is_some();
    let path = global
        .config
        .clone()
        .unwrap_or_else(|| config_file(&dirs, &*p.env));
    let text = read_config_file(&path, explicit)?;
    let managed =
        crate::config::clients_file::read(&dirs.clients_file()).map_err(ConfigErrors::from)?;
    let resolved = resolve(&Inputs {
        file: text.as_deref(),
        file_path: text.as_ref().map(|_| path.clone()),
        profile: global.profile.as_deref(),
        env,
        flags,
        managed_clients: managed.clients,
    })?;
    Ok(Loaded {
        resolved,
        dirs,
        file_text: text,
        file_path: path,
    })
}

/// The gateway token: from `AIRMATE_TOKEN*` if set, else the token file (created on first use).
///
/// # Errors
///
/// The token source cannot be read or the file cannot be created.
pub fn gateway_token(p: &Process, loaded: &Loaded) -> Result<Secret, CliError> {
    match &loaded.resolved.token {
        Some(r) => SystemResolver::new(&*p.env)
            .resolve(r)
            .map(|r| r.secret)
            .map_err(|e| CliError::runtime(format!("cannot read the gateway token: {e}"))),
        None => crate::auth::load_or_create_token(&loaded.dirs.token_file())
            .map_err(|e| CliError::runtime(e.to_string())),
    }
}
