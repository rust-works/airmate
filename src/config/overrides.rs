//! Environment and flag overrides, and the one-upstream quick start.
//!
//! Precedence, highest first: command-line flags, the environment (`AIRMATE_*`), the selected
//! profile, the config file, built-in defaults. Secrets are only ever taken from secret sources.

use super::load::ConfigErrors;
use super::paths::EnvFn;
use super::schema::{
    Config, EffortName, FlavorName, ModelConfig, ProtocolName, SecretRef, StrategyName,
    TargetConfig, UnknownFieldsMode, UpstreamConfig,
};
use super::validate::ConfigError;

/// The settings that make a one-upstream gateway need no file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QuickStart {
    /// `AIRMATE_UPSTREAM_URL` / `--upstream-url`.
    pub upstream_url: Option<String>,
    /// `AIRMATE_UPSTREAM_PROTOCOL` / `--upstream-protocol`.
    pub protocol: Option<ProtocolName>,
    /// `AIRMATE_FLAVOR` / `--flavor`.
    pub flavor: Option<FlavorName>,
    /// `AIRMATE_API_KEY`, `_FILE` or `_COMMAND`.
    pub api_key: Option<SecretRef>,
    /// `AIRMATE_MODEL` / `--model`: the default upstream model.
    pub model: Option<String>,
    /// `AIRMATE_MODEL_MAP` / `--map`: `pattern=model` pairs.
    pub model_map: Vec<(String, String)>,
    /// `AIRMATE_EFFORT_LEVELS`.
    pub effort_levels: Option<Vec<EffortName>>,
}

/// Settings from the environment or flags that sit above the file.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Overrides {
    /// `--bind`.
    pub bind: Option<String>,
    /// `AIRMATE_PORT` / `--port`.
    pub port: Option<u16>,
    /// `AIRMATE_UNKNOWN_FIELDS`.
    pub unknown_fields: Option<UnknownFieldsMode>,
    /// `AIRMATE_THINK_TAGS`.
    pub think_tags: Option<bool>,
    /// `AIRMATE_MAX_CONCURRENT`.
    pub max_concurrent: Option<u32>,
    /// `AIRMATE_TIMEOUT_SECS`: the upstream idle-read timeout.
    pub timeout_secs: Option<u64>,
    /// `--insecure-remote`.
    pub insecure_remote: Option<bool>,
    /// The one-upstream quick start.
    pub quick: QuickStart,
    /// `AIRMATE_TOKEN`, `_FILE` or `_COMMAND`: the inbound token source.
    pub token: Option<SecretRef>,
}

fn bad(var: &str, message: impl Into<String>) -> ConfigErrors {
    ConfigError::new(var, message).into()
}

fn get(env: EnvFn<'_>, name: &str) -> Option<String> {
    env(name).filter(|v| !v.is_empty())
}

fn parse_bool(var: &str, v: &str) -> Result<bool, ConfigErrors> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(bad(
            var,
            format!("`{v}` is not a boolean; use true or false"),
        )),
    }
}

fn parse_num<T: std::str::FromStr>(var: &str, v: &str) -> Result<T, ConfigErrors> {
    v.parse()
        .map_err(|_| bad(var, format!("`{v}` is not a valid number here")))
}

/// Parses `pattern=model,pattern=model`.
///
/// # Errors
///
/// A malformed pair.
pub fn parse_model_map(var: &str, v: &str) -> Result<Vec<(String, String)>, ConfigErrors> {
    let mut out = Vec::new();
    for pair in v.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some((p, m)) if !p.trim().is_empty() && !m.trim().is_empty() => {
                out.push((p.trim().to_owned(), m.trim().to_owned()));
            }
            _ => return Err(bad(var, format!("`{pair}` is not `pattern=model`"))),
        }
    }
    Ok(out)
}

/// Parses a protocol name.
///
/// # Errors
///
/// An unknown name.
pub fn parse_protocol(var: &str, v: &str) -> Result<ProtocolName, ConfigErrors> {
    serde_norway::from_str(v).map_err(|_| {
        bad(
            var,
            format!("`{v}` is not a protocol; use chat, responses or anthropic"),
        )
    })
}

/// Parses a flavor name.
///
/// # Errors
///
/// An unknown name.
pub fn parse_flavor(var: &str, v: &str) -> Result<FlavorName, ConfigErrors> {
    serde_norway::from_str(v).map_err(|_| {
        bad(
            var,
            format!("`{v}` is not a flavor; use generic, openai or fireworks"),
        )
    })
}

/// Parses a comma-separated list of effort levels.
///
/// # Errors
///
/// An unknown level.
pub fn parse_efforts(var: &str, v: &str) -> Result<Vec<EffortName>, ConfigErrors> {
    v.split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(|e| {
            serde_norway::from_str(e).map_err(|_| {
                bad(
                    var,
                    format!("`{e}` is not an effort; use low, medium, high, xhigh or max"),
                )
            })
        })
        .collect()
}

/// A secret from `NAME`, `NAME_FILE` or `NAME_COMMAND`; at most one may be set.
fn secret(env: EnvFn<'_>, name: &str) -> Result<Option<SecretRef>, ConfigErrors> {
    let direct = get(env, name).map(|_| SecretRef::Env(name.to_owned()));
    let file = get(env, &format!("{name}_FILE")).map(SecretRef::File);
    let command = get(env, &format!("{name}_COMMAND"))
        .map(|c| SecretRef::Command(c.split_whitespace().map(str::to_owned).collect()));
    let mut found: Vec<SecretRef> = [direct, file, command].into_iter().flatten().collect();
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => Err(bad(
            name,
            format!("set only one of {name}, {name}_FILE and {name}_COMMAND"),
        )),
    }
}

impl Overrides {
    /// Reads the `AIRMATE_*` variables.
    ///
    /// # Errors
    ///
    /// A variable with a value that does not parse.
    pub fn from_env(env: EnvFn<'_>) -> Result<Self, ConfigErrors> {
        let mut o = Self::default();
        if let Some(v) = get(env, "AIRMATE_PORT") {
            o.port = Some(parse_num("AIRMATE_PORT", &v)?);
        }
        if let Some(v) = get(env, "AIRMATE_UNKNOWN_FIELDS") {
            o.unknown_fields = Some(match v.as_str() {
                "reject" => UnknownFieldsMode::Reject,
                "warn" => UnknownFieldsMode::Warn,
                _ => {
                    return Err(bad(
                        "AIRMATE_UNKNOWN_FIELDS",
                        format!("`{v}` is not reject or warn"),
                    ))
                }
            });
        }
        if let Some(v) = get(env, "AIRMATE_THINK_TAGS") {
            o.think_tags = Some(parse_bool("AIRMATE_THINK_TAGS", &v)?);
        }
        if let Some(v) = get(env, "AIRMATE_MAX_CONCURRENT") {
            o.max_concurrent = Some(parse_num("AIRMATE_MAX_CONCURRENT", &v)?);
        }
        if let Some(v) = get(env, "AIRMATE_TIMEOUT_SECS") {
            o.timeout_secs = Some(parse_num("AIRMATE_TIMEOUT_SECS", &v)?);
        }
        o.token = secret(env, "AIRMATE_TOKEN")?;
        let q = &mut o.quick;
        q.upstream_url = get(env, "AIRMATE_UPSTREAM_URL");
        if let Some(v) = get(env, "AIRMATE_UPSTREAM_PROTOCOL") {
            q.protocol = Some(parse_protocol("AIRMATE_UPSTREAM_PROTOCOL", &v)?);
        }
        if let Some(v) = get(env, "AIRMATE_FLAVOR") {
            q.flavor = Some(parse_flavor("AIRMATE_FLAVOR", &v)?);
        }
        q.api_key = secret(env, "AIRMATE_API_KEY")?;
        q.model = get(env, "AIRMATE_MODEL");
        if let Some(v) = get(env, "AIRMATE_MODEL_MAP") {
            q.model_map = parse_model_map("AIRMATE_MODEL_MAP", &v)?;
        }
        if let Some(v) = get(env, "AIRMATE_EFFORT_LEVELS") {
            q.effort_levels = Some(parse_efforts("AIRMATE_EFFORT_LEVELS", &v)?);
        }
        Ok(o)
    }

    /// `self` with every setting `higher` has set taking its place: flags over the environment.
    #[must_use]
    pub fn over(mut self, higher: Overrides) -> Self {
        macro_rules! take {
            ($($f:ident),*) => { $( if higher.$f.is_some() { self.$f = higher.$f; } )* };
        }
        take!(
            bind,
            port,
            unknown_fields,
            think_tags,
            max_concurrent,
            timeout_secs,
            insecure_remote,
            token
        );
        let (q, h) = (&mut self.quick, higher.quick);
        macro_rules! take_q {
            ($($f:ident),*) => { $( if h.$f.is_some() { q.$f = h.$f; } )* };
        }
        take_q!(
            upstream_url,
            protocol,
            flavor,
            api_key,
            model,
            effort_levels
        );
        if !h.model_map.is_empty() {
            q.model_map = h.model_map;
        }
        self
    }

    /// Applies the overrides to `config`, synthesising the quick-start upstream if asked for.
    ///
    /// # Errors
    ///
    /// The quick start conflicts with upstreams already configured.
    pub fn apply(&self, config: &mut Config) -> Result<(), ConfigErrors> {
        if let Some(b) = &self.bind {
            config.server.bind.clone_from(b);
        }
        if let Some(p) = self.port {
            config.server.port = p;
        }
        if let Some(u) = self.unknown_fields {
            config.server.unknown_fields = u;
        }
        if let Some(t) = self.think_tags {
            config.server.think_tags = t;
        }
        if let Some(n) = self.max_concurrent {
            config.server.max_concurrent = n;
        }
        if let Some(i) = self.insecure_remote {
            config.server.insecure_remote = i;
        }
        if let Some(t) = self.timeout_secs {
            for u in &mut config.upstreams {
                u.timeouts.idle_secs = t;
            }
        }
        self.apply_quick_start(config)
    }

    fn apply_quick_start(&self, config: &mut Config) -> Result<(), ConfigErrors> {
        let q = &self.quick;
        let Some(url) = &q.upstream_url else {
            let stray = q.protocol.is_some()
                || q.flavor.is_some()
                || q.api_key.is_some()
                || q.model.is_some()
                || !q.model_map.is_empty();
            return if stray {
                Err(bad(
                    "AIRMATE_UPSTREAM_URL",
                    "the quick-start settings need AIRMATE_UPSTREAM_URL (or --upstream-url)",
                ))
            } else {
                Ok(())
            };
        };
        if !config.upstreams.is_empty() {
            return Err(bad(
                "AIRMATE_UPSTREAM_URL",
                "the quick-start upstream conflicts with upstreams in the config file; use one or the other",
            ));
        }
        let mut upstream = UpstreamConfig {
            name: "default".to_owned(),
            url: url.clone(),
            protocol: q.protocol.unwrap_or(ProtocolName::Chat),
            flavor: q.flavor.unwrap_or_default(),
            api_key: q.api_key.clone(),
            max_concurrent: None,
            timeouts: Default::default(),
            rate: None,
            health_check: None,
            effort_levels: q.effort_levels.clone(),
            think_tags: None,
            proxy: None,
            aws: None,
        };
        if let Some(t) = self.timeout_secs {
            upstream.timeouts.idle_secs = t;
        }
        config.upstreams.push(upstream);
        let target = |model: &str| TargetConfig {
            upstream: "default".to_owned(),
            model: model.to_owned(),
            weight: None,
            effort_levels: None,
            max_output_tokens: None,
            price: None,
        };
        for (pattern, model) in &q.model_map {
            config.models.push(ModelConfig {
                pattern: pattern.clone(),
                name: None,
                strategy: StrategyName::Fallback,
                targets: vec![target(model)],
            });
        }
        if let Some(model) = &q.model {
            config.models.push(ModelConfig {
                pattern: "*".to_owned(),
                name: None,
                strategy: StrategyName::Fallback,
                targets: vec![target(model)],
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn from(pairs: &[(&str, &str)]) -> Result<Overrides, ConfigErrors> {
        Overrides::from_env(&env(pairs))
    }

    #[test]
    fn an_empty_environment_overrides_nothing() {
        assert_eq!(from(&[]).unwrap(), Overrides::default());
        assert_eq!(
            from(&[("AIRMATE_PORT", "")]).unwrap(),
            Overrides::default(),
            "empty means unset"
        );
    }

    #[test]
    fn scalar_variables_parse() {
        let o = from(&[
            ("AIRMATE_PORT", "8080"),
            ("AIRMATE_UNKNOWN_FIELDS", "warn"),
            ("AIRMATE_THINK_TAGS", "false"),
            ("AIRMATE_MAX_CONCURRENT", "3"),
            ("AIRMATE_TIMEOUT_SECS", "42"),
        ])
        .unwrap();
        assert_eq!(o.port, Some(8080));
        assert_eq!(o.unknown_fields, Some(UnknownFieldsMode::Warn));
        assert_eq!(o.think_tags, Some(false));
        assert_eq!(o.max_concurrent, Some(3));
        assert_eq!(o.timeout_secs, Some(42));
    }

    #[test]
    fn bad_values_name_the_variable() {
        for (var, val) in [
            ("AIRMATE_PORT", "99999"),
            ("AIRMATE_PORT", "x"),
            ("AIRMATE_UNKNOWN_FIELDS", "shrug"),
            ("AIRMATE_THINK_TAGS", "maybe"),
            ("AIRMATE_MAX_CONCURRENT", "-1"),
            ("AIRMATE_TIMEOUT_SECS", "soon"),
            ("AIRMATE_UPSTREAM_PROTOCOL", "grpc"),
            ("AIRMATE_FLAVOR", "azure"),
            ("AIRMATE_MODEL_MAP", "no-equals"),
            ("AIRMATE_EFFORT_LEVELS", "low,extreme"),
        ] {
            let e = from(&[(var, val)]).unwrap_err().to_string();
            assert!(e.starts_with(var), "{var}={val}: {e}");
        }
    }

    #[test]
    fn booleans_accept_the_usual_spellings() {
        for t in ["1", "true", "TRUE", "yes", "on"] {
            assert!(
                from(&[("AIRMATE_THINK_TAGS", t)])
                    .unwrap()
                    .think_tags
                    .unwrap(),
                "{t}"
            );
        }
        for f in ["0", "false", "no", "off"] {
            assert!(
                !from(&[("AIRMATE_THINK_TAGS", f)])
                    .unwrap()
                    .think_tags
                    .unwrap(),
                "{f}"
            );
        }
    }

    #[test]
    fn the_quick_start_variables_parse() {
        let o = from(&[
            ("AIRMATE_UPSTREAM_URL", "http://localhost:11434"),
            ("AIRMATE_UPSTREAM_PROTOCOL", "responses"),
            ("AIRMATE_FLAVOR", "openai"),
            ("AIRMATE_MODEL", "qwen3"),
            (
                "AIRMATE_MODEL_MAP",
                "claude-opus-*=big, claude-haiku-*=small",
            ),
            ("AIRMATE_EFFORT_LEVELS", "low, high"),
        ])
        .unwrap();
        let q = o.quick;
        assert_eq!(q.upstream_url.as_deref(), Some("http://localhost:11434"));
        assert_eq!(q.protocol, Some(ProtocolName::Responses));
        assert_eq!(q.flavor, Some(FlavorName::Openai));
        assert_eq!(q.model.as_deref(), Some("qwen3"));
        assert_eq!(
            q.model_map,
            vec![
                ("claude-opus-*".into(), "big".into()),
                ("claude-haiku-*".into(), "small".into())
            ]
        );
        assert_eq!(
            q.effort_levels,
            Some(vec![EffortName::Low, EffortName::High])
        );
    }

    #[test]
    fn secrets_come_from_exactly_one_source() {
        assert_eq!(
            from(&[("AIRMATE_API_KEY", "k")]).unwrap().quick.api_key,
            Some(SecretRef::Env("AIRMATE_API_KEY".into()))
        );
        assert_eq!(
            from(&[("AIRMATE_API_KEY_FILE", "/f")])
                .unwrap()
                .quick
                .api_key,
            Some(SecretRef::File("/f".into()))
        );
        assert_eq!(
            from(&[("AIRMATE_API_KEY_COMMAND", "pass show fw")])
                .unwrap()
                .quick
                .api_key,
            Some(SecretRef::Command(vec![
                "pass".into(),
                "show".into(),
                "fw".into()
            ]))
        );
        assert_eq!(
            from(&[("AIRMATE_TOKEN_FILE", "/t")]).unwrap().token,
            Some(SecretRef::File("/t".into()))
        );
        assert!(
            from(&[("AIRMATE_API_KEY", "k"), ("AIRMATE_API_KEY_FILE", "/f")])
                .unwrap_err()
                .to_string()
                .contains("only one")
        );
        assert!(from(&[("AIRMATE_TOKEN", "t"), ("AIRMATE_TOKEN_COMMAND", "c")]).is_err());
    }

    #[test]
    fn the_secret_value_itself_never_enters_the_overrides() {
        let o = from(&[
            ("AIRMATE_API_KEY", "super-secret-value"),
            ("AIRMATE_TOKEN", "tok-secret"),
        ])
        .unwrap();
        let shown = format!("{o:?}");
        assert!(
            !shown.contains("super-secret-value") && !shown.contains("tok-secret"),
            "{shown}"
        );
    }

    #[test]
    fn model_maps_reject_malformed_pairs() {
        assert_eq!(parse_model_map("V", "").unwrap(), vec![]);
        assert!(parse_model_map("V", "a=").is_err());
        assert!(parse_model_map("V", "=b").is_err());
        assert!(parse_model_map("V", "a=b,c").is_err());
        assert_eq!(parse_model_map("V", "a=b,,c=d").unwrap().len(), 2);
    }

    #[test]
    fn flags_win_over_the_environment() {
        let envs = Overrides {
            port: Some(1),
            unknown_fields: Some(UnknownFieldsMode::Warn),
            ..Overrides::default()
        };
        let flags = Overrides {
            port: Some(2),
            bind: Some("::1".into()),
            ..Overrides::default()
        };
        let merged = envs.over(flags);
        assert_eq!(merged.port, Some(2));
        assert_eq!(merged.bind.as_deref(), Some("::1"));
        assert_eq!(
            merged.unknown_fields,
            Some(UnknownFieldsMode::Warn),
            "unset flags do not erase the environment"
        );
    }

    #[test]
    fn quick_start_fields_merge_field_by_field() {
        let mut e = Overrides::default();
        e.quick.upstream_url = Some("http://env".into());
        e.quick.model = Some("env-model".into());
        e.quick.model_map = vec![("a".into(), "b".into())];
        let mut f = Overrides::default();
        f.quick.model = Some("flag-model".into());
        let m = e.over(f);
        assert_eq!(m.quick.upstream_url.as_deref(), Some("http://env"));
        assert_eq!(m.quick.model.as_deref(), Some("flag-model"));
        assert_eq!(m.quick.model_map.len(), 1);
        let mut f2 = Overrides::default();
        f2.quick.model_map = vec![("x".into(), "y".into())];
        assert_eq!(m.over(f2).quick.model_map, vec![("x".into(), "y".into())]);
    }

    #[test]
    fn overrides_apply_over_the_file() {
        let mut c = Config::default();
        c.server.port = 1234;
        let o = Overrides {
            port: Some(9000),
            bind: Some("::1".into()),
            think_tags: Some(false),
            max_concurrent: Some(2),
            insecure_remote: Some(true),
            unknown_fields: Some(UnknownFieldsMode::Warn),
            ..Overrides::default()
        };
        o.apply(&mut c).unwrap();
        assert_eq!((c.server.port, c.server.bind.as_str()), (9000, "::1"));
        assert!(!c.server.think_tags && c.server.insecure_remote);
        assert_eq!(c.server.max_concurrent, 2);
        assert_eq!(c.server.unknown_fields, UnknownFieldsMode::Warn);
        Overrides::default().apply(&mut c).unwrap();
        assert_eq!(c.server.port, 9000, "no override leaves it alone");
    }

    #[test]
    fn the_quick_start_builds_one_upstream_and_its_model_entries() {
        let o = Overrides {
            timeout_secs: Some(77),
            quick: QuickStart {
                upstream_url: Some("http://localhost:11434".into()),
                protocol: Some(ProtocolName::Anthropic),
                flavor: Some(FlavorName::Fireworks),
                api_key: Some(SecretRef::Env("K".into())),
                model: Some("fallback-model".into()),
                model_map: vec![("claude-opus-*".into(), "big".into())],
                effort_levels: Some(vec![EffortName::Low]),
            },
            ..Overrides::default()
        };
        let mut c = Config::default();
        o.apply(&mut c).unwrap();
        assert_eq!(c.upstreams.len(), 1);
        let u = &c.upstreams[0];
        assert_eq!(
            (u.name.as_str(), u.protocol, u.flavor),
            ("default", ProtocolName::Anthropic, FlavorName::Fireworks)
        );
        assert_eq!(u.timeouts.idle_secs, 77);
        assert_eq!(u.effort_levels, Some(vec![EffortName::Low]));
        let patterns: Vec<_> = c.models.iter().map(|m| m.pattern.as_str()).collect();
        assert_eq!(
            patterns,
            ["claude-opus-*", "*"],
            "the map comes before the catch-all"
        );
        assert!(crate::config::validate(&c).is_empty());
    }

    #[test]
    fn the_quick_start_defaults_to_chat_generic_and_no_catch_all_without_a_model() {
        let mut o = Overrides::default();
        o.quick.upstream_url = Some("http://x:1".into());
        let mut c = Config::default();
        o.apply(&mut c).unwrap();
        assert_eq!(
            (c.upstreams[0].protocol, c.upstreams[0].flavor),
            (ProtocolName::Chat, FlavorName::Generic)
        );
        assert!(c.models.is_empty());
    }

    #[test]
    fn the_quick_start_conflicts_with_a_configured_upstream_and_needs_a_url() {
        let mut o = Overrides::default();
        o.quick.upstream_url = Some("http://x:1".into());
        let mut c = Config::default();
        o.apply(&mut c).unwrap();
        let e = o.apply(&mut c).unwrap_err().to_string();
        assert!(e.contains("conflicts"), "{e}");
        let mut stray = Overrides::default();
        stray.quick.model = Some("m".into());
        assert!(stray
            .apply(&mut Config::default())
            .unwrap_err()
            .to_string()
            .contains("need AIRMATE_UPSTREAM_URL"));
    }
}
