//! Resolving the effective configuration, and saying where each value came from.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::Value as Json;

use super::load::{load_str, parse, ConfigErrors};
use super::overrides::Overrides;
use super::schema::{Config, SecretRef};
use super::validate::{validate, ConfigError};

/// Where a value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The built-in default.
    Default,
    /// The config file.
    File,
    /// A profile in the config file.
    Profile(String),
    /// An `AIRMATE_*` environment variable.
    Env,
    /// A command-line flag.
    Flag,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::File => f.write_str("file"),
            Self::Profile(p) => write!(f, "profile `{p}`"),
            Self::Env => f.write_str("environment"),
            Self::Flag => f.write_str("flag"),
        }
    }
}

/// What to resolve.
#[derive(Debug, Clone, Default)]
pub struct Inputs<'a> {
    /// The config file's text, if there is one.
    pub file: Option<&'a str>,
    /// Where the file was read from.
    pub file_path: Option<PathBuf>,
    /// The selected profile.
    pub profile: Option<&'a str>,
    /// Settings from the environment.
    pub env: Overrides,
    /// Settings from flags.
    pub flags: Overrides,
    /// Clients from the managed `clients.yaml`, merged into the configuration.
    pub managed_clients: Vec<super::schema::ClientConfig>,
}

/// The effective configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The configuration, validated.
    pub config: Config,
    /// Where the inbound token comes from, if the environment or a flag said.
    pub token: Option<SecretRef>,
    /// The config file, if one was read.
    pub file: Option<PathBuf>,
    /// The profile applied.
    pub profile: Option<String>,
    /// The source of every value that is not a default.
    pub sources: BTreeMap<String, Source>,
}

fn flatten(prefix: &str, v: &Json, out: &mut BTreeMap<String, String>) {
    match v {
        Json::Object(m) => {
            for (k, v) in m {
                let p = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten(&p, v, out);
            }
        }
        Json::Array(a) => {
            if a.is_empty() {
                out.insert(prefix.to_owned(), "[]".to_owned());
            }
            for (i, v) in a.iter().enumerate() {
                flatten(&format!("{prefix}[{i}]"), v, out);
            }
        }
        Json::Null => {}
        other => {
            out.insert(prefix.to_owned(), other.to_string());
        }
    }
}

/// The settings of a configuration as `path -> JSON value`.
#[must_use]
pub fn flat(c: &Config) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    flatten(
        "",
        &serde_json::to_value(c).expect("a config serialises"),
        &mut out,
    );
    out
}

/// Resolves the configuration from the file, profile, environment and flags.
///
/// # Errors
///
/// A syntax error, an unknown profile, a conflicting quick start, or any validation problem.
pub fn resolve(inputs: &Inputs<'_>) -> Result<Resolved, ConfigErrors> {
    let defaults = Config::default();
    let file_layer = match inputs.file {
        Some(text) => parse(text, None)?,
        None => defaults.clone(),
    };
    let profile_layer = match (inputs.file, inputs.profile) {
        (Some(text), Some(p)) => parse(text, Some(p))?,
        (None, Some(p)) => {
            return Err(ConfigError::new(
                "profile",
                format!("no profile named `{p}`; there is no config file"),
            )
            .into());
        }
        _ => {
            let mut c = file_layer.clone();
            c.profiles.clear();
            c
        }
    };
    let mut file_layer = file_layer;
    file_layer.profiles.clear();

    let mut env_layer = profile_layer.clone();
    inputs.env.apply(&mut env_layer)?;
    let merged = inputs.env.clone().over(inputs.flags.clone());
    let mut final_layer = profile_layer.clone();
    merged.apply(&mut final_layer)?;

    super::clients_file::merge(&mut final_layer.clients, &inputs.managed_clients)
        .map_err(ConfigErrors::from)?;
    let problems = validate(&final_layer);
    if !problems.is_empty() {
        return Err(ConfigErrors(problems));
    }

    let layers: [(Source, BTreeMap<String, String>); 5] = [
        (Source::Default, flat(&defaults)),
        (Source::File, flat(&file_layer)),
        (
            Source::Profile(inputs.profile.unwrap_or_default().to_owned()),
            flat(&profile_layer),
        ),
        (Source::Env, flat(&env_layer)),
        (Source::Flag, flat(&final_layer)),
    ];
    let mut sources: BTreeMap<String, Source> = BTreeMap::new();
    let mut previous = &layers[0].1;
    for (source, values) in &layers[1..] {
        for (path, value) in values {
            if previous.get(path) != Some(value) {
                sources.insert(path.clone(), source.clone());
            }
        }
        previous = values;
    }

    Ok(Resolved {
        config: final_layer,
        token: merged.token,
        file: inputs.file_path.clone(),
        profile: inputs.profile.map(str::to_owned),
        sources,
    })
}

/// The effective configuration as lines of `path = value  # source`.
#[must_use]
pub fn show(r: &Resolved) -> String {
    let mut out = String::new();
    for (path, value) in flat(&r.config) {
        let source = r.sources.get(&path).cloned().unwrap_or(Source::Default);
        out.push_str(&format!("{path} = {value}  # {source}\n"));
    }
    out
}

/// Reads the config file.
///
/// # Errors
///
/// The file cannot be read, or it is missing and `explicit` says the user named it.
pub fn read_config_file(path: &Path, explicit: bool) -> Result<Option<String>, ConfigErrors> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => Ok(None),
        Err(e) => Err(ConfigError::new(
            path.display().to_string(),
            format!("cannot read the config file: {e}"),
        )
        .into()),
    }
}

/// Convenience for `config check`: parse and validate the file and every profile in it.
///
/// # Errors
///
/// Every problem found.
pub fn check(text: &str) -> Result<Config, ConfigErrors> {
    super::load::check_all(text)
}

/// Convenience for loading without overrides.
///
/// # Errors
///
/// As [`load_str`].
pub fn load(text: &str, profile: Option<&str>) -> Result<Config, ConfigErrors> {
    load_str(text, profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::overrides::Overrides;
    use crate::config::schema::UnknownFieldsMode;

    const FILE: &str = "server:\n  port: 8000\n  max_concurrent: 4\nprofiles:\n  work:\n    server: { port: 9100 }\n";

    fn inputs<'a>(file: Option<&'a str>, profile: Option<&'a str>) -> Inputs<'a> {
        Inputs {
            file,
            file_path: Some(PathBuf::from("/x/airmate.yaml")),
            profile,
            ..Inputs::default()
        }
    }

    #[test]
    fn without_a_file_the_defaults_apply_and_the_port_is_9997() {
        let r = resolve(&Inputs::default()).unwrap();
        assert_eq!(r.config.server.port, 9997);
        assert!(r.sources.is_empty());
        assert_eq!(r.file, None);
    }

    #[test]
    fn precedence_is_flags_then_env_then_profile_then_file_then_defaults() {
        let r = resolve(&inputs(Some(FILE), None)).unwrap();
        assert_eq!(r.config.server.port, 8000);
        assert_eq!(r.sources["server.port"], Source::File);

        let r = resolve(&inputs(Some(FILE), Some("work"))).unwrap();
        assert_eq!(r.config.server.port, 9100);
        assert_eq!(r.sources["server.port"], Source::Profile("work".into()));
        assert_eq!(
            r.sources["server.max_concurrent"],
            Source::File,
            "the profile left it alone"
        );

        let mut i = inputs(Some(FILE), Some("work"));
        i.env = Overrides {
            port: Some(7000),
            ..Overrides::default()
        };
        let r = resolve(&i).unwrap();
        assert_eq!(
            (r.config.server.port, &r.sources["server.port"]),
            (7000, &Source::Env)
        );

        i.flags = Overrides {
            port: Some(6000),
            ..Overrides::default()
        };
        let r = resolve(&i).unwrap();
        assert_eq!(
            (r.config.server.port, &r.sources["server.port"]),
            (6000, &Source::Flag)
        );
    }

    #[test]
    fn the_environment_can_set_things_the_file_does_not_mention() {
        let mut i = inputs(None, None);
        i.env = Overrides {
            unknown_fields: Some(UnknownFieldsMode::Warn),
            ..Overrides::default()
        };
        let r = resolve(&i).unwrap();
        assert_eq!(r.sources["server.unknown_fields"], Source::Env);
        assert_eq!(r.config.server.unknown_fields, UnknownFieldsMode::Warn);
    }

    #[test]
    fn the_quick_start_resolves_without_a_file() {
        let mut i = inputs(None, None);
        i.env.quick.upstream_url = Some("http://localhost:11434".into());
        i.env.quick.model = Some("qwen3".into());
        i.flags.port = Some(1234);
        let r = resolve(&i).unwrap();
        assert_eq!(r.config.upstreams[0].url, "http://localhost:11434");
        assert_eq!(r.config.models[0].targets[0].model, "qwen3");
        assert_eq!(r.sources["upstreams[0].url"], Source::Env);
        assert_eq!(r.sources["server.port"], Source::Flag);
    }

    #[test]
    fn the_token_source_comes_from_the_environment_or_a_flag() {
        let mut i = inputs(None, None);
        i.env.token = Some(SecretRef::File("/env".into()));
        assert_eq!(
            resolve(&i).unwrap().token,
            Some(SecretRef::File("/env".into()))
        );
        i.flags.token = Some(SecretRef::File("/flag".into()));
        assert_eq!(
            resolve(&i).unwrap().token,
            Some(SecretRef::File("/flag".into()))
        );
    }

    #[test]
    fn resolution_validates_the_final_result() {
        let mut i = inputs(None, None);
        i.env.max_concurrent = Some(0);
        assert!(resolve(&i)
            .unwrap_err()
            .to_string()
            .contains("max_concurrent"));
        let mut i = inputs(None, None);
        i.flags.bind = Some("0.0.0.0".into());
        assert!(resolve(&i)
            .unwrap_err()
            .to_string()
            .contains("allowed_hosts"));
    }

    #[test]
    fn a_profile_without_a_file_is_an_error() {
        let e = resolve(&inputs(None, Some("work")))
            .unwrap_err()
            .to_string();
        assert!(e.contains("no config file"), "{e}");
        let e = resolve(&inputs(Some(FILE), Some("nope")))
            .unwrap_err()
            .to_string();
        assert!(e.contains("work"), "{e}");
    }

    #[test]
    fn show_lists_every_value_with_its_source() {
        let mut i = inputs(Some(FILE), None);
        i.env.port = Some(7000);
        let text = show(&resolve(&i).unwrap());
        assert!(
            text.contains("server.port = 7000  # environment\n"),
            "{text}"
        );
        assert!(
            text.contains("server.max_concurrent = 4  # file\n"),
            "{text}"
        );
        assert!(
            text.contains("server.bind = \"127.0.0.1\"  # default\n"),
            "{text}"
        );
        assert!(
            text.contains("server.think_tags = true  # default\n"),
            "{text}"
        );
    }

    #[test]
    fn show_never_prints_a_secret_literal() {
        let yaml = "upstreams:\n  - name: a\n    url: http://x\n    api_key: { value: hunter2 }\n";
        let text = show(&resolve(&inputs(Some(yaml), None)).unwrap());
        assert!(!text.contains("hunter2"), "{text}");
        assert!(text.contains("<redacted>"));
    }

    #[test]
    fn source_names_read_well() {
        assert_eq!(Source::Default.to_string(), "default");
        assert_eq!(Source::Profile("w".into()).to_string(), "profile `w`");
        assert_eq!(Source::Flag.to_string(), "flag");
    }

    #[test]
    fn reading_the_config_file_distinguishes_missing_from_unreadable() {
        let dir = std::env::temp_dir().join(format!("airmate-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.yaml");
        std::fs::write(&p, "server: {}\n").unwrap();
        assert_eq!(
            read_config_file(&p, true).unwrap().as_deref(),
            Some("server: {}\n")
        );
        let missing = dir.join("missing.yaml");
        assert_eq!(read_config_file(&missing, false).unwrap(), None);
        assert!(read_config_file(&missing, true)
            .unwrap_err()
            .to_string()
            .contains("cannot read"));
        assert!(
            read_config_file(&dir, false).is_err(),
            "a directory is not a config file"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_conveniences_delegate() {
        assert!(check("server: {}\n").is_ok());
        assert_eq!(load("server: { port: 5 }\n", None).unwrap().server.port, 5);
    }
}
