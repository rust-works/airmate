//! Configuration validation.
//!
//! Every problem is collected, with the path of the setting, so `airmate config check` can report
//! them all at once rather than one per run.

use std::collections::HashSet;
use std::fmt;
use std::net::IpAddr;

use super::schema::{Config, ProtocolName, RuleConfig, StrategyName, TargetConfig, UpstreamConfig};

/// A problem with a setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    /// The setting, e.g. `upstreams[fireworks].url`.
    pub path: String,
    /// What is wrong.
    pub message: String,
}

impl ConfigError {
    /// A new problem.
    #[must_use]
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl std::error::Error for ConfigError {}

/// Whether `bind` is a loopback address.
#[must_use]
pub fn is_loopback(bind: &str) -> bool {
    bind.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The base URL with a trailing `/v1` (and slashes) dropped; airmate appends `/v1/...`.
#[must_use]
pub fn normalize_url(url: &str) -> String {
    let t = url.trim_end_matches('/');
    t.strip_suffix("/v1")
        .unwrap_or(t)
        .trim_end_matches('/')
        .to_owned()
}

fn url_problem(url: &str) -> Option<&'static str> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"));
    match rest {
        None => Some("must start with http:// or https://"),
        Some(r) if r.split('/').next().is_none_or(str::is_empty) => Some("has no host"),
        Some(r) if r.contains(char::is_whitespace) => Some("must not contain whitespace"),
        Some(_) => None,
    }
}

fn valid_key_hash(h: &str) -> bool {
    h.strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

fn check_targets(
    path: &str,
    targets: &[TargetConfig],
    weighted: bool,
    upstreams: &HashSet<&str>,
    out: &mut Vec<ConfigError>,
) {
    if targets.is_empty() {
        out.push(ConfigError::new(
            format!("{path}.targets"),
            "needs at least one target",
        ));
    }
    for (i, t) in targets.iter().enumerate() {
        let tp = format!("{path}.targets[{i}]");
        if !upstreams.contains(t.upstream.as_str()) {
            out.push(ConfigError::new(
                format!("{tp}.upstream"),
                format!("`{}` is not a configured upstream", t.upstream),
            ));
        }
        if t.model.is_empty() {
            out.push(ConfigError::new(format!("{tp}.model"), "must not be empty"));
        }
        if weighted && t.weight == Some(0) {
            out.push(ConfigError::new(
                format!("{tp}.weight"),
                "must be at least 1",
            ));
        }
        if let Some(p) = &t.price {
            if p.input_per_mtok < 0.0 || p.output_per_mtok < 0.0 {
                out.push(ConfigError::new(
                    format!("{tp}.price"),
                    "prices must not be negative",
                ));
            }
        }
    }
}

fn check_upstream(u: &UpstreamConfig, out: &mut Vec<ConfigError>) {
    let p = format!("upstreams[{}]", u.name);
    if !name_ok(&u.name) {
        out.push(ConfigError::new(
            format!("upstreams[{}].name", u.name),
            "must be non-empty and use only letters, digits, `_`, `-` and `.`",
        ));
    }
    if let Some(problem) = url_problem(&u.url) {
        out.push(ConfigError::new(format!("{p}.url"), problem));
    }
    if u.timeouts.connect_secs == 0 || u.timeouts.first_byte_secs == 0 || u.timeouts.idle_secs == 0
    {
        out.push(ConfigError::new(
            format!("{p}.timeouts"),
            "timeouts must be at least 1 second",
        ));
    }
    if u.max_concurrent == Some(0) {
        out.push(ConfigError::new(
            format!("{p}.max_concurrent"),
            "must be at least 1",
        ));
    }
    if let Some(h) = &u.health_check {
        if h.interval_secs == 0 || !h.path.starts_with('/') {
            out.push(ConfigError::new(
                format!("{p}.health_check"),
                "needs a path starting with `/` and an interval of at least 1 second",
            ));
        }
    }
    if let Some(r) = &u.rate {
        if r.rpm == Some(0) || r.tpm == Some(0) {
            out.push(ConfigError::new(
                format!("{p}.rate"),
                "limits must be at least 1",
            ));
        }
    }
    if let Some(super::schema::SecretRef::Command(c)) = &u.api_key {
        if c.is_empty() {
            out.push(ConfigError::new(
                format!("{p}.api_key.command"),
                "needs at least the program to run",
            ));
        }
    }
    if u.proxy.is_some() {
        out.push(ConfigError::new(
            format!("{p}.proxy"),
            "upstream proxies are not supported yet",
        ));
    }
    match (&u.aws, u.protocol) {
        (Some(_), proto) if proto != ProtocolName::Bedrock => out.push(ConfigError::new(
            format!("{p}.aws"),
            "AWS credentials apply to `protocol: bedrock` upstreams only",
        )),
        (Some(a), _) => {
            if a.region.is_none() && crate::router::table::region_from_url(&u.url).is_none() {
                out.push(ConfigError::new(
                    format!("{p}.aws.region"),
                    "set `region`: it cannot be read from the URL",
                ));
            }
            if u.api_key.is_some() {
                out.push(ConfigError::new(
                    format!("{p}.aws"),
                    "use either `aws` (signed requests) or `api_key` (a Bedrock API key), not both",
                ));
            }
        }
        (None, ProtocolName::Bedrock) if u.api_key.is_none() => out.push(ConfigError::new(
            format!("{p}.api_key"),
            "a Bedrock upstream needs `aws` credentials or an `api_key` (a Bedrock API key)",
        )),
        (None, _) => {}
    }
    if matches!(u.protocol, ProtocolName::Gemini | ProtocolName::Bedrock)
        && u.flavor != super::schema::FlavorName::Generic
    {
        out.push(ConfigError::new(
            format!("{p}.flavor"),
            "flavors apply to the chat, responses and anthropic protocols",
        ));
    }
}

fn check_rule(
    r: &RuleConfig,
    models: &HashSet<&str>,
    upstreams: &HashSet<&str>,
    out: &mut Vec<ConfigError>,
) {
    let p = format!("rules[{}]", r.name);
    match (&r.use_entry, &r.targets) {
        (Some(_), Some(_)) => out.push(ConfigError::new(
            &p,
            "set either `use` or `targets`, not both",
        )),
        (None, None) => out.push(ConfigError::new(
            &p,
            "needs `use` (a model entry) or `targets`",
        )),
        (Some(u), None) => {
            if !models.contains(u.as_str()) {
                out.push(ConfigError::new(
                    format!("{p}.use"),
                    format!("`{u}` is not a model entry"),
                ));
            }
        }
        (None, Some(t)) => check_targets(&p, t, false, upstreams, out),
    }
    let w = &r.when;
    if let (Some(a), Some(b)) = (w.effort_min, w.effort_max) {
        if airmate_core::ir::Effort::from(a) > airmate_core::ir::Effort::from(b) {
            out.push(ConfigError::new(
                format!("{p}.when"),
                "effort_min is above effort_max",
            ));
        }
    }
    if let (Some(a), Some(b)) = (w.est_input_tokens_min, w.est_input_tokens_max) {
        if a > b {
            out.push(ConfigError::new(
                format!("{p}.when"),
                "est_input_tokens_min is above est_input_tokens_max",
            ));
        }
    }
    if let (Some(a), Some(b)) = (w.max_tokens_min, w.max_tokens_max) {
        if a > b {
            out.push(ConfigError::new(
                format!("{p}.when"),
                "max_tokens_min is above max_tokens_max",
            ));
        }
    }
    if let Some(h) = w.hour_of_day {
        if h.from > 23 || h.to > 24 {
            out.push(ConfigError::new(
                format!("{p}.when.hour_of_day"),
                "`from` must be 0 to 23 and `to` 0 to 24",
            ));
        }
    }
}

/// Validates a configuration, returning every problem found.
#[must_use]
pub fn validate(c: &Config) -> Vec<ConfigError> {
    let mut out = Vec::new();

    let s = &c.server;
    match s.bind.parse::<IpAddr>() {
        Err(_) => out.push(ConfigError::new(
            "server.bind",
            format!("`{}` is not an IP address", s.bind),
        )),
        Ok(_) if !is_loopback(&s.bind) => {
            if s.allowed_hosts.is_empty() {
                out.push(ConfigError::new(
                    "server.allowed_hosts",
                    "a non-loopback bind needs an `allowed_hosts` list",
                ));
            }
            if s.tls.is_none() && !s.insecure_remote {
                out.push(ConfigError::new("server.bind", "a non-loopback bind needs `tls` or `insecure_remote: true` acknowledging the risk"));
            }
        }
        Ok(_) => {}
    }
    if s.max_concurrent == 0 {
        out.push(ConfigError::new(
            "server.max_concurrent",
            "must be at least 1",
        ));
    }
    if s.max_body_bytes == 0 {
        out.push(ConfigError::new(
            "server.max_body_bytes",
            "must be at least 1",
        ));
    }
    if s.keepalive_secs == 0 {
        out.push(ConfigError::new(
            "server.keepalive_secs",
            "must be at least 1",
        ));
    }
    if s.buffer_cap_bytes == 0 {
        out.push(ConfigError::new(
            "server.buffer_cap_bytes",
            "must be at least 1",
        ));
    }

    let mut seen = HashSet::new();
    for u in &c.upstreams {
        if !seen.insert(u.name.as_str()) {
            out.push(ConfigError::new(
                format!("upstreams[{}]", u.name),
                "duplicate upstream name",
            ));
        }
        check_upstream(u, &mut out);
    }
    let upstreams: HashSet<&str> = c.upstreams.iter().map(|u| u.name.as_str()).collect();

    let mut entry_names = HashSet::new();
    for (i, m) in c.models.iter().enumerate() {
        let p = format!("models[{i}]");
        if m.pattern.is_empty() {
            out.push(ConfigError::new(format!("{p}.match"), "must not be empty"));
        }
        if !entry_names.insert(m.entry_name()) {
            out.push(ConfigError::new(
                &p,
                format!("duplicate model entry name `{}`", m.entry_name()),
            ));
        }
        check_targets(
            &p,
            &m.targets,
            m.strategy == StrategyName::Weighted,
            &upstreams,
            &mut out,
        );
    }
    let models: HashSet<&str> = c.models.iter().map(|m| m.entry_name()).collect();

    let mut classifier_names = HashSet::new();
    for cl in &c.classifiers {
        if !name_ok(&cl.name) || !classifier_names.insert(cl.name.as_str()) {
            out.push(ConfigError::new(
                format!("classifiers[{}]", cl.name),
                "classifier names must be unique and use only letters, digits, `_`, `-` and `.`",
            ));
        }
        if let Some(problem) = url_problem(&cl.url) {
            out.push(ConfigError::new(
                format!("classifiers[{}].url", cl.name),
                problem,
            ));
        }
        if cl.timeout_ms == 0 {
            out.push(ConfigError::new(
                format!("classifiers[{}].timeout_ms", cl.name),
                "must be at least 1",
            ));
        }
    }
    let allowed_headers: HashSet<String> = c
        .server
        .rule_headers
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect();
    let mut rule_names = HashSet::new();
    for r in &c.rules {
        if r.name.is_empty() || !rule_names.insert(r.name.as_str()) {
            out.push(ConfigError::new(
                format!("rules[{}]", r.name),
                "rule names must be non-empty and unique",
            ));
        }
        check_rule(r, &models, &upstreams, &mut out);
        for h in r.when.header.keys() {
            if !allowed_headers.contains(&h.to_ascii_lowercase()) {
                out.push(ConfigError::new(
                    format!("rules[{}].when.header.{h}", r.name),
                    "add the header to `server.rule_headers` to let rules match on it",
                ));
            }
        }
        if let Some(cm) = &r.when.classifier {
            if !classifier_names.contains(cm.name.as_str()) {
                out.push(ConfigError::new(
                    format!("rules[{}].when.classifier", r.name),
                    format!("`{}` is not a configured classifier", cm.name),
                ));
            }
        }
    }

    let mut client_names = HashSet::new();
    for cl in &c.clients {
        let p = format!("clients[{}]", cl.name);
        if !name_ok(&cl.name) || !client_names.insert(cl.name.as_str()) {
            out.push(ConfigError::new(
                &p,
                "client names must be unique and use only letters, digits, `_`, `-` and `.`",
            ));
        }
        if !valid_key_hash(&cl.key_hash) {
            out.push(ConfigError::new(
                format!("{p}.key_hash"),
                "must be `sha256:` followed by 64 hex digits; `airmate keys add` writes it",
            ));
        }
        if let Some(f) = &cl.budget_fallback {
            if !models.contains(f.as_str()) {
                out.push(ConfigError::new(
                    format!("{p}.budget_fallback"),
                    format!("`{f}` is not a model entry"),
                ));
            }
        }
        if let Some(b) = &cl.budget {
            for (k, v) in [
                ("usd_per_day", b.usd_per_day),
                ("usd_per_week", b.usd_per_week),
                ("usd_per_month", b.usd_per_month),
            ] {
                if v.is_some_and(|v| v < 0.0 || v.is_nan()) {
                    out.push(ConfigError::new(
                        format!("{p}.budget.{k}"),
                        "must not be negative",
                    ));
                }
            }
        }
    }

    for (i, o) in c.catalog.iter().enumerate() {
        if o.pattern.is_empty() {
            out.push(ConfigError::new(
                format!("catalog[{i}].match"),
                "must not be empty",
            ));
        }
        if o.context_window == Some(0) || o.max_output_tokens == Some(0) {
            out.push(ConfigError::new(
                format!("catalog[{i}]"),
                "context_window and max_output_tokens must be at least 1",
            ));
        }
    }
    if c.circuit.failure_threshold == 0 {
        out.push(ConfigError::new(
            "circuit.failure_threshold",
            "must be at least 1",
        ));
    }
    if c.circuit.window == 0 || c.circuit.min_samples > c.circuit.window {
        out.push(ConfigError::new(
            "circuit.window",
            "must be at least 1 and not below min_samples",
        ));
    }
    if !(0.0..=1.0).contains(&c.circuit.error_rate) {
        out.push(ConfigError::new(
            "circuit.error_rate",
            "must be between 0 and 1",
        ));
    }
    if c.circuit.max_cooldown_secs < c.circuit.cooldown_secs {
        out.push(ConfigError::new(
            "circuit.max_cooldown_secs",
            "must not be below cooldown_secs",
        ));
    }
    if c.failover.max_attempts == 0 {
        out.push(ConfigError::new(
            "failover.max_attempts",
            "must be at least 1",
        ));
    }
    if c.failover.backoff_cap_ms < c.failover.backoff_base_ms {
        out.push(ConfigError::new(
            "failover.backoff_cap_ms",
            "must not be below backoff_base_ms",
        ));
    }
    if c.usage.prices.is_empty() {
        out.push(ConfigError::new(
            "usage.prices",
            "must be `builtin` or the path of a price file",
        ));
    }
    if let Some(r) = &c.log.rotate {
        if r.keep == 0 || r.max_size_mb == 0 {
            out.push(ConfigError::new(
                "log.rotate",
                "`max_size_mb` and `keep` must be at least 1",
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::*;

    fn upstream(name: &str) -> UpstreamConfig {
        UpstreamConfig {
            name: name.into(),
            url: "http://localhost:11434".into(),
            protocol: ProtocolName::Chat,
            flavor: FlavorName::Generic,
            api_key: None,
            max_concurrent: None,
            timeouts: Timeouts::default(),
            rate: None,
            health_check: None,
            effort_levels: None,
            think_tags: None,
            proxy: None,
            aws: None,
        }
    }

    fn model(pattern: &str, up: &str) -> ModelConfig {
        ModelConfig {
            pattern: pattern.into(),
            name: None,
            strategy: StrategyName::Fallback,
            targets: vec![TargetConfig {
                upstream: up.into(),
                model: "m".into(),
                weight: None,
                effort_levels: None,
                max_output_tokens: None,
                price: None,
            }],
        }
    }

    fn good() -> Config {
        Config {
            upstreams: vec![upstream("local")],
            models: vec![model("*", "local")],
            ..Config::default()
        }
    }

    fn problems(c: &Config) -> Vec<String> {
        validate(c).iter().map(ToString::to_string).collect()
    }

    fn has(c: &Config, needle: &str) -> bool {
        problems(c).iter().any(|p| p.contains(needle))
    }

    #[test]
    fn the_default_and_a_minimal_config_are_valid() {
        assert!(validate(&Config::default()).is_empty());
        assert!(validate(&good()).is_empty());
    }

    #[test]
    fn a_non_loopback_bind_needs_hosts_and_tls_or_an_acknowledgement() {
        let mut c = good();
        c.server.bind = "0.0.0.0".into();
        assert!(has(&c, "allowed_hosts") && has(&c, "insecure_remote"));
        c.server.allowed_hosts = vec!["airmate.example".into()];
        c.server.insecure_remote = true;
        assert!(validate(&c).is_empty());
        c.server.insecure_remote = false;
        c.server.tls = Some(TlsConfig {
            cert: "c".into(),
            key: "k".into(),
        });
        assert!(validate(&c).is_empty());
        c.server.bind = "::1".into();
        c.server.tls = None;
        c.server.allowed_hosts.clear();
        assert!(validate(&c).is_empty(), "::1 is loopback");
        c.server.bind = "localhost".into();
        assert!(has(&c, "not an IP address"));
    }

    #[test]
    fn upstream_urls_and_names_are_checked() {
        for (url, msg) in [
            ("ftp://x", "http://"),
            ("http://", "no host"),
            ("http:///path", "no host"),
            ("http://a b", "whitespace"),
        ] {
            let mut c = good();
            c.upstreams[0].url = url.into();
            assert!(has(&c, msg), "{url}");
        }
        let mut c = good();
        c.upstreams[0].name = "bad name!".into();
        assert!(has(&c, "letters, digits"));
        c.upstreams = vec![upstream("a"), upstream("a")];
        c.models.clear();
        assert!(has(&c, "duplicate upstream"));
    }

    #[test]
    fn upstream_numbers_are_checked() {
        let mut c = good();
        c.upstreams[0].timeouts.idle_secs = 0;
        c.upstreams[0].max_concurrent = Some(0);
        c.upstreams[0].rate = Some(RateLimit {
            rpm: Some(0),
            tpm: None,
        });
        c.upstreams[0].health_check = Some(HealthCheck {
            path: "nope".into(),
            interval_secs: 0,
        });
        c.upstreams[0].api_key = Some(SecretRef::Command(vec![]));
        let p = problems(&c).join("\n");
        for needle in [
            "timeouts",
            "max_concurrent",
            "rate",
            "health_check",
            "api_key.command",
        ] {
            assert!(p.contains(needle), "{needle}\n{p}");
        }
    }

    #[test]
    fn flavors_do_not_apply_to_gemini_or_bedrock() {
        let mut c = good();
        c.upstreams[0].protocol = ProtocolName::Gemini;
        c.upstreams[0].flavor = FlavorName::Fireworks;
        assert!(has(&c, "flavors apply"));
    }

    #[test]
    fn bedrock_needs_credentials_and_a_region_and_aws_applies_only_to_bedrock() {
        use crate::config::schema::AwsConfig;
        let aws = |region: Option<&str>| AwsConfig {
            region: region.map(str::to_owned),
            access_key_id: SecretRef::Env("AK".into()),
            secret_access_key: SecretRef::Env("SK".into()),
            session_token: None,
        };
        let mut c = good();
        c.upstreams[0].protocol = ProtocolName::Bedrock;
        c.upstreams[0].url = "https://bedrock-runtime.us-west-2.amazonaws.com".into();
        assert!(has(&c, "needs `aws` credentials or an `api_key`"));
        c.upstreams[0].aws = Some(aws(None));
        assert!(
            problems(&c).is_empty(),
            "the region is read from the URL: {:?}",
            problems(&c)
        );
        c.upstreams[0].url = "http://localhost:4566".into();
        assert!(has(&c, "set `region`"));
        c.upstreams[0].aws = Some(aws(Some("us-east-1")));
        assert!(problems(&c).is_empty());
        c.upstreams[0].api_key = Some(SecretRef::Env("K".into()));
        assert!(has(&c, "either `aws`"));
        c.upstreams[0].api_key = None;
        c.upstreams[0].protocol = ProtocolName::Chat;
        assert!(has(&c, "`protocol: bedrock` upstreams only"));
        let mut key_only = good();
        key_only.upstreams[0].protocol = ProtocolName::Bedrock;
        key_only.upstreams[0].api_key = Some(SecretRef::Env("K".into()));
        assert!(
            problems(&key_only).is_empty(),
            "a Bedrock API key is enough"
        );
    }

    #[test]
    fn models_must_name_real_upstreams_and_have_targets() {
        let mut c = good();
        c.models[0].targets[0].upstream = "ghost".into();
        assert!(has(&c, "`ghost` is not a configured upstream"));
        c.models[0].targets.clear();
        assert!(has(&c, "at least one target"));
        let mut c = good();
        c.models[0].pattern.clear();
        c.models[0].targets[0].model.clear();
        assert!(has(&c, "models[0].match") && has(&c, "models[0].targets[0].model"));
        let mut c = good();
        c.models.push(model("*", "local"));
        assert!(has(&c, "duplicate model entry"));
    }

    #[test]
    fn weights_and_prices_are_checked() {
        let mut c = good();
        c.models[0].strategy = StrategyName::Weighted;
        c.models[0].targets[0].weight = Some(0);
        assert!(has(&c, "weight"));
        c.models[0].strategy = StrategyName::Fallback;
        assert!(
            !has(&c, "weight"),
            "weights only matter to the weighted strategy"
        );
        c.models[0].targets[0].price = Some(Price {
            input_per_mtok: -1.0,
            output_per_mtok: 1.0,
            cache_read_per_mtok: None,
        });
        assert!(has(&c, "negative"));
    }

    #[test]
    fn rules_need_exactly_one_action_that_exists() {
        let rule = |use_entry: Option<&str>, targets: bool| RuleConfig {
            name: "r".into(),
            when: RuleWhen::default(),
            use_entry: use_entry.map(str::to_owned),
            targets: targets.then(|| model("x", "local").targets),
        };
        let mut c = good();
        c.rules = vec![rule(None, false)];
        assert!(has(&c, "needs `use`"));
        c.rules = vec![rule(Some("*"), true)];
        assert!(has(&c, "not both"));
        c.rules = vec![rule(Some("nope"), false)];
        assert!(has(&c, "not a model entry"));
        c.rules = vec![rule(Some("*"), false)];
        assert!(validate(&c).is_empty());
        c.rules = vec![rule(None, true)];
        assert!(validate(&c).is_empty());
        c.rules = vec![rule(Some("*"), false), rule(Some("*"), false)];
        assert!(has(&c, "unique"));
    }

    #[test]
    fn rule_predicates_are_checked_for_contradictions() {
        let mut c = good();
        let mut r = RuleConfig {
            name: "r".into(),
            when: RuleWhen::default(),
            use_entry: Some("*".into()),
            targets: None,
        };
        r.when.effort_min = Some(EffortName::High);
        r.when.effort_max = Some(EffortName::Low);
        r.when.est_input_tokens_min = Some(10);
        r.when.est_input_tokens_max = Some(1);
        r.when.max_tokens_min = Some(10);
        r.when.max_tokens_max = Some(1);
        r.when.hour_of_day = Some(HourRange { from: 24, to: 25 });
        c.rules = vec![r];
        let p = problems(&c).join("\n");
        for needle in [
            "effort_min",
            "est_input_tokens_min",
            "max_tokens_min",
            "hour_of_day",
        ] {
            assert!(p.contains(needle), "{needle}");
        }
    }

    #[test]
    fn clients_need_unique_names_valid_hashes_and_real_fallbacks() {
        let client = |name: &str, hash: &str| ClientConfig {
            name: name.into(),
            key_hash: hash.into(),
            models: vec![],
            rate: None,
            budget: None,
            budget_fallback: None,
        };
        let ok_hash = format!("sha256:{}", "a".repeat(64));
        let mut c = good();
        c.clients = vec![client("ci", &ok_hash)];
        assert!(validate(&c).is_empty());
        c.clients = vec![client("ci", &ok_hash), client("ci", &ok_hash)];
        assert!(has(&c, "unique"));
        c.clients = vec![client("ci", "sha256:short"), client("md5", "md5:abc")];
        assert!(has(&c, "64 hex digits"));
        let mut cl = client("ci", &ok_hash);
        cl.budget_fallback = Some("ghost".into());
        cl.budget = Some(Budget {
            usd_per_day: Some(-1.0),
            usd_per_week: None,
            usd_per_month: None,
        });
        c.clients = vec![cl];
        assert!(has(&c, "budget_fallback") && has(&c, "usd_per_day"));
    }

    #[test]
    fn limits_and_logging_are_checked() {
        let mut c = good();
        c.server.max_concurrent = 0;
        c.server.max_body_bytes = 0;
        c.server.buffer_cap_bytes = 0;
        c.circuit.failure_threshold = 0;
        c.failover.max_attempts = 0;
        c.failover.backoff_base_ms = 10;
        c.failover.backoff_cap_ms = 1;
        c.usage.prices.clear();
        c.log.rotate = Some(Rotate {
            max_size_mb: 0,
            keep: 0,
        });
        let p = problems(&c).join("\n");
        for needle in [
            "max_concurrent",
            "max_body_bytes",
            "buffer_cap_bytes",
            "failure_threshold",
            "max_attempts",
            "backoff_cap_ms",
            "usage.prices",
            "log.rotate",
        ] {
            assert!(p.contains(needle), "{needle}");
        }
    }

    #[test]
    fn urls_normalise_by_dropping_a_trailing_v1() {
        for (input, want) in [
            ("https://api.openai.com/v1", "https://api.openai.com"),
            ("https://api.openai.com/v1/", "https://api.openai.com"),
            ("http://localhost:11434/", "http://localhost:11434"),
            (
                "https://api.fireworks.ai/inference/v1",
                "https://api.fireworks.ai/inference",
            ),
            ("https://x.example/v10", "https://x.example/v10"),
        ] {
            assert_eq!(normalize_url(input), want, "{input}");
        }
    }

    #[test]
    fn errors_display_with_their_path() {
        assert_eq!(ConfigError::new("a.b", "bad").to_string(), "a.b: bad");
        assert_eq!(ConfigError::new("", "bad").to_string(), "bad");
    }
}
