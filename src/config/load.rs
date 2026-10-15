//! Reading the YAML configuration, with profiles.

use std::collections::BTreeMap;
use std::fmt;

use serde_norway::{Mapping, Value};

use super::schema::Config;
use super::validate::{validate, ConfigError};

/// Everything wrong with a configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErrors(pub Vec<ConfigError>);

impl fmt::Display for ConfigErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigErrors {}

impl From<ConfigError> for ConfigErrors {
    fn from(e: ConfigError) -> Self {
        Self(vec![e])
    }
}

fn one(path: &str, message: impl Into<String>) -> ConfigErrors {
    ConfigError::new(path, message).into()
}

/// Merges `over` into `base`: mappings merge recursively, everything else is replaced.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Mapping(b), Value::Mapping(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

/// The error text with its line and column, when the parser knows them.
fn located(e: &serde_norway::Error) -> String {
    match e.location() {
        Some(l) => format!("{e} (line {}, column {})", l.line(), l.column()),
        None => e.to_string(),
    }
}

fn profiles_of(root: &Value) -> Result<BTreeMap<String, Value>, ConfigErrors> {
    let mut out = BTreeMap::new();
    let Some(Value::Mapping(m)) = root.as_mapping().and_then(|m| m.get("profiles")).cloned() else {
        return Ok(out);
    };
    for (k, v) in m {
        let name = k
            .as_str()
            .ok_or_else(|| one("profiles", "profile names must be strings"))?
            .to_owned();
        if !v.is_mapping() && !v.is_null() {
            return Err(one(
                &format!("profiles.{name}"),
                "a profile must be a mapping of overrides",
            ));
        }
        out.insert(name, v);
    }
    Ok(out)
}

/// Parses `yaml` and applies the profile `profile`, if any, without validating the result.
///
/// # Errors
///
/// A syntax error, an unknown key (with its line when no profile is applied), or a profile that
/// does not exist.
pub fn parse(yaml: &str, profile: Option<&str>) -> Result<Config, ConfigErrors> {
    let mut root: Value = if yaml.trim().is_empty() {
        Value::Mapping(Mapping::new())
    } else {
        serde_norway::from_str(yaml)
            .map_err(|e| one("", format!("invalid YAML: {}", located(&e))))?
    };
    if root.is_null() {
        root = Value::Mapping(Mapping::new());
    }
    if !root.is_mapping() {
        return Err(one(
            "",
            "the configuration must be a mapping at the top level",
        ));
    }
    let profiles = profiles_of(&root)?;
    let Some(name) = profile else {
        // Without a profile the file is read as written, so errors carry their line.
        return serde_norway::from_str::<Config>(yaml).map_err(|e| one("", located(&e)));
    };
    let over = profiles.get(name).cloned().ok_or_else(|| {
        let known: Vec<&str> = profiles.keys().map(String::as_str).collect();
        one(
            "profile",
            if known.is_empty() {
                format!("no profile named `{name}`; the file defines none")
            } else {
                format!(
                    "no profile named `{name}`; the file defines: {}",
                    known.join(", ")
                )
            },
        )
    })?;
    if let Some(m) = root.as_mapping_mut() {
        m.remove("profiles");
    }
    if !over.is_null() {
        merge(&mut root, over);
    }
    serde_norway::from_value::<Config>(root)
        .map_err(|e| one(&format!("profiles.{name}"), e.to_string()))
}

/// Parses, applies the profile and validates.
///
/// # Errors
///
/// Every problem found, with its setting.
pub fn load_str(yaml: &str, profile: Option<&str>) -> Result<Config, ConfigErrors> {
    let config = parse(yaml, profile)?;
    let problems = validate(&config);
    if problems.is_empty() {
        Ok(config)
    } else {
        Err(ConfigErrors(problems))
    }
}

/// Validates the base configuration and every profile applied over it. Used by `config check`.
///
/// # Errors
///
/// Every problem found, prefixed with the profile it appears in.
pub fn check_all(yaml: &str) -> Result<Config, ConfigErrors> {
    let base = load_str(yaml, None)?;
    let mut problems = Vec::new();
    for name in base.profiles.keys() {
        match load_str(yaml, Some(name)) {
            Ok(_) => {}
            Err(ConfigErrors(es)) => problems.extend(
                es.into_iter()
                    .map(|e| ConfigError::new(format!("profile `{name}`: {}", e.path), e.message)),
            ),
        }
    }
    if problems.is_empty() {
        Ok(base)
    } else {
        Err(ConfigErrors(problems))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{FlavorName, ProtocolName, DEFAULT_PORT};

    const FULL: &str = r#"
server:
  bind: 127.0.0.1
  port: 8123
  max_concurrent: 4
  unknown_fields: warn
upstreams:
  - name: fireworks
    url: https://api.fireworks.ai/inference
    protocol: anthropic
    flavor: fireworks
    api_key: { command: [pass, show, fireworks] }
    timeouts: { connect_secs: 5, first_byte_secs: 30, idle_secs: 120 }
  - name: local
    url: http://localhost:11434
    health_check: { path: /api/tags, interval_secs: 15 }
models:
  - match: "claude-opus-*"
    targets:
      - { upstream: fireworks, model: accounts/fireworks/models/kimi-k2p6 }
      - { upstream: local, model: qwen3 }
  - match: "*"
    name: default
    targets: [ { upstream: local, model: qwen3-coder } ]
profiles:
  work:
    server: { port: 9100 }
    upstreams:
      - name: local
        url: http://localhost:1234
    models:
      - { match: "*", targets: [ { upstream: local, model: m } ] }
"#;

    #[test]
    fn an_empty_file_is_the_defaults_with_port_9997() {
        for text in ["", "   \n", "# only a comment\n", "{}"] {
            let c = load_str(text, None).unwrap();
            assert_eq!(c.server.port, DEFAULT_PORT, "{text:?}");
            assert_eq!(c.server.port, 9997);
            assert_eq!(c.server.bind, "127.0.0.1");
            assert_eq!(c, Config::default());
        }
    }

    #[test]
    fn the_port_is_configurable_in_the_file() {
        let c = load_str("server:\n  port: 8123\n", None).unwrap();
        assert_eq!(c.server.port, 8123);
        let c = load_str("server:\n  port: 0\n", None).unwrap();
        assert_eq!(c.server.port, 0, "0 asks for a free port");
        assert!(load_str("server:\n  port: 70000\n", None).is_err());
        assert!(load_str("server:\n  port: ninety\n", None).is_err());
    }

    #[test]
    fn a_full_file_parses() {
        let c = load_str(FULL, None).unwrap();
        assert_eq!(c.server.port, 8123);
        assert_eq!(c.server.max_concurrent, 4);
        assert_eq!(c.upstreams.len(), 2);
        assert_eq!(c.upstreams[0].protocol, ProtocolName::Anthropic);
        assert_eq!(c.upstreams[0].flavor, FlavorName::Fireworks);
        assert_eq!(c.upstreams[0].timeouts.idle_secs, 120);
        assert_eq!(
            c.upstreams[1].protocol,
            ProtocolName::Chat,
            "chat is the default protocol"
        );
        assert_eq!(c.models[0].targets.len(), 2);
        assert_eq!(c.models[1].entry_name(), "default");
        assert_eq!(c.profiles.len(), 1);
    }

    #[test]
    fn unknown_keys_are_errors_not_silently_ignored() {
        for text in [
            "serverr: {}\n",
            "server:\n  prot: 1\n",
            "upstreams:\n  - name: a\n    url: http://x\n    flavour: openai\n",
        ] {
            let e = load_str(text, None).unwrap_err().to_string();
            assert!(e.contains("unknown field"), "{text:?}: {e}");
        }
    }

    #[test]
    fn errors_carry_a_line_number_when_no_profile_is_applied() {
        let e = load_str("server:\n  bind: 127.0.0.1\n  prot: 1\n", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("line 3"), "{e}");
    }

    #[test]
    fn invalid_yaml_and_wrong_shapes_are_reported() {
        assert!(load_str("server: [unclosed", None)
            .unwrap_err()
            .to_string()
            .contains("invalid YAML"));
        assert!(load_str("- a\n- b\n", None)
            .unwrap_err()
            .to_string()
            .contains("mapping at the top level"));
        assert!(load_str("upstreams: nope\n", None).is_err());
    }

    #[test]
    fn a_profile_overrides_scalars_and_replaces_lists() {
        let c = load_str(FULL, Some("work")).unwrap();
        assert_eq!(c.server.port, 9100, "scalar overridden");
        assert_eq!(c.server.max_concurrent, 4, "untouched siblings are kept");
        assert_eq!(c.upstreams.len(), 1, "lists are replaced, not merged");
        assert_eq!(c.upstreams[0].url, "http://localhost:1234");
        assert_eq!(c.models.len(), 1);
        assert!(c.profiles.is_empty(), "profiles are consumed");
    }

    #[test]
    fn an_unknown_profile_names_the_available_ones() {
        let e = load_str(FULL, Some("nope")).unwrap_err().to_string();
        assert!(
            e.contains("no profile named `nope`") && e.contains("work"),
            "{e}"
        );
        let e = load_str("", Some("x")).unwrap_err().to_string();
        assert!(e.contains("defines none"), "{e}");
    }

    #[test]
    fn profiles_must_be_mappings_and_may_be_empty() {
        let e = load_str("profiles:\n  a: 3\n", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("profiles.a"), "{e}");
        let c = load_str("profiles:\n  a:\n", Some("a")).unwrap();
        assert_eq!(c.server.port, 9997);
    }

    #[test]
    fn a_profile_with_a_bad_key_is_reported_against_the_profile() {
        let e = load_str("profiles:\n  a:\n    server: { prot: 1 }\n", Some("a"))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("profiles.a") && e.contains("unknown field"),
            "{e}"
        );
    }

    #[test]
    fn validation_problems_are_all_reported_together() {
        let text = "server:\n  max_concurrent: 0\n  max_body_bytes: 0\n";
        let e = load_str(text, None).unwrap_err();
        assert_eq!(e.0.len(), 2);
        assert!(e.to_string().contains('\n'));
    }

    #[test]
    fn check_all_validates_every_profile() {
        assert!(check_all(FULL).is_ok());
        let bad = "profiles:\n  broken:\n    server: { max_concurrent: 0 }\n";
        let e = check_all(bad).unwrap_err().to_string();
        assert!(
            e.contains("profile `broken`") && e.contains("max_concurrent"),
            "{e}"
        );
        assert!(check_all("server: { max_concurrent: 0 }\n").is_err());
    }

    #[test]
    fn secrets_are_references_and_literals_never_print() {
        let text = "upstreams:\n  - name: a\n    url: http://x\n    api_key: { value: hunter2 }\n  - name: b\n    url: http://y\n    api_key: { env: B_KEY }\n  - name: c\n    url: http://z\n    api_key: { file: /run/secrets/c }\n";
        let c = load_str(text, None).unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        let yaml = serde_norway::to_string(&c).unwrap();
        assert!(!yaml.contains("hunter2"), "{yaml}");
        assert!(yaml.contains("<redacted>") && yaml.contains("B_KEY"));
    }

    #[test]
    fn merge_replaces_non_mappings_and_merges_mappings() {
        let mut base: Value = serde_norway::from_str("a: {x: 1, y: 2}\nl: [1, 2]\n").unwrap();
        merge(
            &mut base,
            serde_norway::from_str("a: {y: 3, z: 4}\nl: [9]\nn: 1\n").unwrap(),
        );
        let want: Value = serde_norway::from_str("a: {x: 1, y: 3, z: 4}\nl: [9]\nn: 1\n").unwrap();
        assert_eq!(base, want);
    }

    #[test]
    fn a_secret_takes_exactly_one_source() {
        let up =
            |key: &str| format!("upstreams:\n  - name: a\n    url: http://x\n    api_key: {key}\n");
        assert!(load_str(&up("{ env: K }"), None).is_ok());
        assert!(load_str(&up("{ keychain: svc }"), None).is_ok());
        let e = load_str(&up("{ env: K, file: /f }"), None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("exactly one"), "{e}");
        let e = load_str(&up("{}"), None).unwrap_err().to_string();
        assert!(e.contains("needs one of"), "{e}");
        assert!(load_str(&up("{ passwd: x }"), None).is_err());
        assert!(load_str(&up("{ command: [pass, show] }"), None).is_ok());
    }

    #[test]
    fn config_errors_convert_and_display() {
        let e: ConfigErrors = ConfigError::new("p", "m").into();
        assert_eq!(e.to_string(), "p: m");
    }
}
