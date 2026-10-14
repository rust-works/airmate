//! The command line.

pub mod common;
pub mod env_cmd;
pub mod run_cmd;
pub mod serve;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};

use crate::config::overrides::{parse_flavor, parse_model_map, parse_protocol, Overrides};
use crate::config::resolve::show;
use crate::config::schema::{SecretRef, DEFAULT_PORT};
use airmate_core::ir::Protocol;
use airmate_core::policy;
use common::{gateway_token, load, CliError, Global, Process};
use env_cmd::{base_url, render, variables, Family, Format};

/// airmate: a local, default-closed AI router.
#[derive(Debug, Parser)]
#[command(name = "airmate", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// The config file (default: `$AIRMATE_CONFIG`, else the platform config directory).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Apply the named profile from the config file.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// The subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the gateway in the foreground.
    Serve(ServeArgs),
    /// Print the settings that point a client at the gateway.
    Env(EnvArgs),
    /// Run a command with its client settings pointed at the gateway.
    Run(RunArgs),
    /// Print the field policy table.
    Fields(FieldsArgs),
    /// Inspect and check the configuration.
    Config {
        /// What to do.
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Print a shell completion script.
    #[cfg(feature = "completions")]
    Completions {
        /// The shell.
        shell: clap_complete::Shell,
    },
    /// Print the version.
    Version,
}

/// Settings for the listener and the one-upstream quick start.
#[derive(Debug, Args, Default)]
pub struct ServeArgs {
    /// The address to bind (default 127.0.0.1).
    #[arg(long)]
    pub bind: Option<String>,
    /// The port (default 9997; 0 picks a free one).
    #[arg(long)]
    pub port: Option<u16>,
    /// Acknowledge the risk of a non-loopback bind without TLS.
    #[arg(long)]
    pub insecure_remote: bool,
    /// Quick start: the upstream's base URL.
    #[arg(long, value_name = "URL")]
    pub upstream_url: Option<String>,
    /// Quick start: chat, responses or anthropic.
    #[arg(long, value_name = "PROTOCOL")]
    pub upstream_protocol: Option<String>,
    /// Quick start: generic, openai or fireworks.
    #[arg(long, value_name = "FLAVOR")]
    pub flavor: Option<String>,
    /// Quick start: the default upstream model.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
    /// Quick start: `pattern=model` (repeatable).
    #[arg(long = "map", value_name = "PATTERN=MODEL")]
    pub map: Vec<String>,
}

/// Which client family `env` prints for.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum InboundArg {
    /// Anthropic SDKs and Claude Code.
    Anthropic,
    /// OpenAI Chat Completions clients.
    Chat,
    /// OpenAI Responses clients.
    Responses,
}

/// `env` options.
#[derive(Debug, Args)]
pub struct EnvArgs {
    /// shell, json or dotenv.
    #[arg(long, value_enum, default_value = "shell")]
    pub format: FormatArg,
    /// The client family.
    #[arg(long, value_enum, default_value = "anthropic")]
    pub inbound: InboundArg,
    /// Use this named client's saved key instead of the gateway token.
    #[arg(long, value_name = "NAME")]
    pub client: Option<String>,
}

/// Output format choices for `env`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum FormatArg {
    /// `export NAME=value`
    Shell,
    /// `{"env": {...}}`
    Json,
    /// `NAME=value`
    Dotenv,
}

/// `run` options.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Use this named client's saved key instead of the gateway token.
    #[arg(long, value_name = "NAME")]
    pub client: Option<String>,
    /// The command to run, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

/// `fields` options.
#[derive(Debug, Args)]
pub struct FieldsArgs {
    /// anthropic, chat or responses.
    #[arg(long, value_enum, default_value = "anthropic")]
    pub inbound: InboundArg,
    /// md or json.
    #[arg(long, value_enum, default_value = "md")]
    pub format: TableFormat,
}

/// Output format of the field table.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TableFormat {
    /// Markdown.
    Md,
    /// JSON.
    Json,
}

/// `config` actions.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Validate the file, every profile in it, and resolve every secret reference.
    Check,
    /// Print the effective configuration with the source of every value.
    Show,
    /// Print the config file path.
    Path,
}

/// Runs the command line; returns the exit code.
pub fn run<I, T>(args: I, p: &Process, out: &mut dyn Write, err: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let code = if e.use_stderr() {
                common::EXIT_USAGE
            } else {
                0
            };
            let text = e.render().to_string();
            let _ = if e.use_stderr() {
                write!(err, "{text}")
            } else {
                write!(out, "{text}")
            };
            return code;
        }
    };
    let global = Global {
        config: cli.config.clone(),
        profile: cli.profile.clone(),
    };
    match dispatch(&cli.command, &global, p, out, err) {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "airmate: {e}");
            e.code
        }
    }
}

fn dispatch(
    cmd: &Command,
    global: &Global,
    p: &Process,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<(), CliError> {
    match cmd {
        Command::Serve(a) => serve::serve(p, global, flags_of(a)?, err),
        Command::Env(a) => env_command(p, global, a, out),
        Command::Run(a) => run_cmd::run(p, global, a.client.as_deref(), &a.command, err),
        Command::Fields(a) => fields(a, out),
        Command::Config { action } => config_command(p, global, action, out),
        #[cfg(feature = "completions")]
        Command::Completions { shell } => {
            clap_complete::generate(*shell, &mut Cli::command(), "airmate", out);
            Ok(())
        }
        Command::Version => {
            writeln!(out, "airmate {}", env!("CARGO_PKG_VERSION")).map_err(io)?;
            Ok(())
        }
    }
}

fn io(e: std::io::Error) -> CliError {
    CliError::runtime(format!("cannot write output: {e}"))
}

/// Turns the serve flags into overrides.
///
/// # Errors
///
/// A flag value that does not parse.
pub fn flags_of(a: &ServeArgs) -> Result<Overrides, CliError> {
    let mut o = Overrides {
        bind: a.bind.clone(),
        port: a.port,
        ..Overrides::default()
    };
    if a.insecure_remote {
        o.insecure_remote = Some(true);
    }
    o.quick.upstream_url.clone_from(&a.upstream_url);
    o.quick.model.clone_from(&a.model);
    if let Some(v) = &a.upstream_protocol {
        o.quick.protocol = Some(parse_protocol("--upstream-protocol", v)?);
    }
    if let Some(v) = &a.flavor {
        o.quick.flavor = Some(parse_flavor("--flavor", v)?);
    }
    for m in &a.map {
        o.quick.model_map.extend(parse_model_map("--map", m)?);
    }
    Ok(o)
}

fn env_command(
    p: &Process,
    global: &Global,
    a: &EnvArgs,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    let loaded = load(p, global, Overrides::default())?;
    let base = base_url(&loaded.resolved.config)?;
    let token = match &a.client {
        Some(name) => run_cmd::client_key(p, &loaded.dirs, name)?,
        None => gateway_token(p, &loaded)?,
    };
    let family = match a.inbound {
        InboundArg::Anthropic => Family::Anthropic,
        InboundArg::Chat | InboundArg::Responses => Family::OpenAi,
    };
    let format = match a.format {
        FormatArg::Shell => Format::Shell,
        FormatArg::Json => Format::Json,
        FormatArg::Dotenv => Format::Dotenv,
    };
    render(&variables(family, &base, token.expose()), format, out).map_err(io)
}

fn fields(a: &FieldsArgs, out: &mut dyn Write) -> Result<(), CliError> {
    let protocol = match a.inbound {
        InboundArg::Anthropic => Protocol::Anthropic,
        InboundArg::Chat => Protocol::Chat,
        InboundArg::Responses => Protocol::Responses,
    };
    let table = policy::table(protocol);
    if table.is_empty() {
        return Err(CliError::usage(format!(
            "there is no field policy table for the {} inbound yet",
            protocol.as_str()
        )));
    }
    let text = match a.format {
        TableFormat::Md => policy::render_markdown(table),
        TableFormat::Json => policy::render_json(table),
    };
    writeln!(out, "{}", text.trim_end()).map_err(io)
}

fn describe(r: &SecretRef) -> String {
    match r {
        SecretRef::Env(n) => format!("environment variable {n}"),
        SecretRef::File(p) => format!("file {p}"),
        SecretRef::Command(c) => format!(
            "credential helper `{}`",
            c.first().map_or("", String::as_str)
        ),
        SecretRef::Keychain(s) => format!("keychain entry {s}"),
        SecretRef::Value(_) => "a literal in the config file".to_owned(),
    }
}

fn config_command(
    p: &Process,
    global: &Global,
    action: &ConfigAction,
    out: &mut dyn Write,
) -> Result<(), CliError> {
    match action {
        ConfigAction::Path => {
            let dirs = p.dirs();
            let path = global
                .config
                .clone()
                .unwrap_or_else(|| crate::config::config_file(&dirs, &*p.env));
            let state = if path.is_file() {
                "exists"
            } else {
                "does not exist"
            };
            writeln!(out, "{} ({state})", path.display()).map_err(io)
        }
        ConfigAction::Show => {
            let loaded = load(p, global, Overrides::default())?;
            write!(out, "{}", show(&loaded.resolved)).map_err(io)
        }
        ConfigAction::Check => {
            let loaded = load(p, global, Overrides::default())?;
            if let Some(text) = &loaded.file_text {
                crate::config::check_all(text)?;
            }
            let config = std::sync::Arc::new(loaded.resolved.config.clone());
            let resolver = crate::secret::SystemResolver::new(&*p.env);
            let (_, warnings) = crate::router::ModelTable::build(&config, &resolver)
                .map_err(|errs| CliError::usage(crate::config::ConfigErrors(errs).to_string()))?;
            writeln!(
                out,
                "config ok: {} upstream(s), {} model entr{}, {} rule(s), {} client(s)",
                config.upstreams.len(),
                config.models.len(),
                if config.models.len() == 1 { "y" } else { "ies" },
                config.rules.len(),
                config.clients.len()
            )
            .map_err(io)?;
            for u in &config.upstreams {
                match &u.api_key {
                    Some(r) => writeln!(
                        out,
                        "  upstreams[{}].api_key resolved from {}",
                        u.name,
                        describe(r)
                    )
                    .map_err(io)?,
                    None => writeln!(out, "  upstreams[{}] has no api_key", u.name).map_err(io)?,
                }
            }
            if let Some(f) = loaded.resolved.file.as_ref() {
                writeln!(out, "  file: {}", f.display()).map_err(io)?;
            }
            for w in warnings {
                writeln!(out, "warning: {w}").map_err(io)?;
            }
            let _ = DEFAULT_PORT;
            Ok(())
        }
    }
}
