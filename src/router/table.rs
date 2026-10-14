//! The runtime tables: upstreams with their secrets resolved, and the model table that turns the
//! name a client sent into a model entry.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use airmate_core::flavor::{is_reasoning_family, Flavor};
use airmate_core::ir::{Effort, Protocol};
use airmate_core::outbound::Target;

use super::glob::{glob_match, is_glob};
use crate::config::schema::{Config, Price, StrategyName, Timeouts, UpstreamConfig};
use crate::config::validate::{normalize_url, ConfigError};
use crate::secret::{Secret, SecretResolver};
use crate::server::upstream::{build_client, HttpClient};

/// An upstream, ready to call.
#[derive(Debug)]
pub struct UpstreamRt {
    /// The name.
    pub name: String,
    /// The base URL without a trailing `/v1`.
    pub base_url: String,
    /// The protocol it speaks.
    pub protocol: Protocol,
    /// Its flavor.
    pub flavor: Flavor,
    /// Its API key, if it has one.
    pub api_key: Option<Secret>,
    /// Timeouts.
    pub timeouts: Timeouts,
    /// Overrides the accepted effort levels.
    pub effort_levels: Option<Vec<Effort>>,
    /// Whether a leading `<think>` block is split out as reasoning.
    pub think_tags: bool,
    /// The config setting that holds the key, for error messages.
    pub key_setting: String,
    /// The per-upstream concurrency limit.
    pub max_concurrent: Option<u32>,
    /// The pooled HTTP client for this upstream.
    pub client: HttpClient,
    /// Limits concurrent calls to this upstream, when configured.
    pub slots: Option<Arc<tokio::sync::Semaphore>>,
    /// The active health check, when configured: `(path, interval)`.
    pub health_check: Option<(String, Duration)>,
    /// Request and token rate limits for this upstream.
    pub rate: Option<crate::config::schema::RateLimit>,
}

impl UpstreamRt {
    /// How long to wait for the connection to open.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_secs(self.timeouts.connect_secs)
    }
}

/// One (upstream, model) target of a model entry.
#[derive(Debug, Clone)]
pub struct RouteTarget {
    /// The upstream.
    pub upstream: Arc<UpstreamRt>,
    /// The model name to send upstream.
    pub model: String,
    /// Overrides the accepted effort levels.
    pub effort_levels: Option<Vec<Effort>>,
    /// Caps the output tokens sent upstream.
    pub max_output_tokens: Option<u32>,
    /// The weight for the `weighted` strategy.
    pub weight: u32,
    /// An explicit price.
    pub price: Option<Price>,
}

impl RouteTarget {
    /// The target as the translation core sees it.
    #[must_use]
    pub fn core_target(&self, think_tags_default: bool) -> Target {
        Target {
            upstream: self.upstream.name.clone(),
            protocol: self.upstream.protocol,
            flavor: self.upstream.flavor,
            model: self.model.clone(),
            effort_levels: self
                .effort_levels
                .clone()
                .or_else(|| self.upstream.effort_levels.clone()),
            max_output_tokens_cap: self.max_output_tokens,
            think_tags: self.upstream.think_tags && think_tags_default,
        }
    }

    /// `upstream/model`, for logs and headers.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}/{}", self.upstream.name, self.model)
    }
}

/// A model entry: what a client names.
#[derive(Debug, Clone)]
pub struct ModelEntry {
    /// The pattern the client's model name is matched against.
    pub pattern: String,
    /// The name rules and clients use.
    pub name: String,
    /// How targets are ordered.
    pub strategy: StrategyName,
    /// The targets, in configured order.
    pub targets: Vec<RouteTarget>,
}

/// What a client's model name resolved to.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// A model entry from the table.
    Entry(Arc<ModelEntry>),
    /// A direct upstream model name (`upstream/model`, or just `model` for the default upstream).
    Direct(Arc<ModelEntry>),
    /// A name in the inbound protocol's own family with nothing to answer it.
    NotFound,
}

/// How a name came to resolve to its entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Via {
    /// The name is an exact entry in the table.
    Exact,
    /// The name matched a glob.
    Glob(String),
    /// The name is a direct upstream model name.
    Direct,
    /// Nothing else matched, so the default entry answered.
    Default,
}

impl std::fmt::Display for Via {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact => f.write_str("an exact name"),
            Self::Glob(g) => write!(f, "the glob `{g}`"),
            Self::Direct => f.write_str("a direct upstream model name"),
            Self::Default => f.write_str("the default entry"),
        }
    }
}

/// The model table.
#[derive(Debug, Default)]
pub struct ModelTable {
    entries: Vec<Arc<ModelEntry>>,
    upstreams: HashMap<String, Arc<UpstreamRt>>,
    default_upstream: Option<Arc<UpstreamRt>>,
}

/// Whether `name` is in the inbound protocol's own model-name family.
#[must_use]
pub fn in_own_family(inbound: Protocol, name: &str) -> bool {
    match inbound {
        Protocol::Anthropic => name.starts_with("claude-") || name == "claude",
        Protocol::Chat | Protocol::Responses => {
            name.starts_with("gpt-") || name.starts_with("chatgpt-") || is_reasoning_family(name)
        }
        Protocol::Gemini => name.starts_with("gemini-"),
        Protocol::Bedrock => false,
    }
}

impl ModelTable {
    /// Builds the table, resolving every upstream's secret.
    ///
    /// # Errors
    ///
    /// A secret that cannot be resolved.
    pub fn build(
        config: &Config,
        resolver: &dyn SecretResolver,
    ) -> Result<(Self, Vec<String>), Vec<ConfigError>> {
        let mut errors = Vec::new();
        let mut warnings = Vec::new();
        let mut upstreams: HashMap<String, Arc<UpstreamRt>> = HashMap::new();
        let mut first = None;
        for u in &config.upstreams {
            match upstream_rt(u, config, resolver) {
                Ok((rt, warning)) => {
                    warnings.extend(warning);
                    let rt = Arc::new(rt);
                    first.get_or_insert_with(|| rt.clone());
                    upstreams.insert(u.name.clone(), rt);
                }
                Err(e) => errors.push(e),
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut entries = Vec::new();
        for m in &config.models {
            let targets = m
                .targets
                .iter()
                .filter_map(|t| {
                    let up = upstreams.get(&t.upstream)?.clone();
                    Some(RouteTarget {
                        upstream: up,
                        model: t.model.clone(),
                        effort_levels: t
                            .effort_levels
                            .as_ref()
                            .map(|l| l.iter().map(|e| (*e).into()).collect()),
                        max_output_tokens: t.max_output_tokens,
                        weight: t.weight.unwrap_or(1),
                        price: t.price,
                    })
                })
                .collect();
            entries.push(Arc::new(ModelEntry {
                pattern: m.pattern.clone(),
                name: m.entry_name().to_owned(),
                strategy: m.strategy,
                targets,
            }));
        }
        let default_upstream = upstreams.get("default").cloned().or(first);
        Ok((
            Self {
                entries,
                upstreams,
                default_upstream,
            },
            warnings,
        ))
    }

    /// The entries in configured order.
    #[must_use]
    pub fn entries(&self) -> &[Arc<ModelEntry>] {
        &self.entries
    }

    /// An entry by its name.
    #[must_use]
    pub fn entry_by_name(&self, name: &str) -> Option<&Arc<ModelEntry>> {
        self.entries.iter().find(|e| e.name == name)
    }

    /// An upstream by name.
    #[must_use]
    pub fn upstream(&self, name: &str) -> Option<&Arc<UpstreamRt>> {
        self.upstreams.get(name)
    }

    /// Every upstream.
    pub fn upstreams(&self) -> impl Iterator<Item = &Arc<UpstreamRt>> {
        self.upstreams.values()
    }

    /// Resolves a model name, in order: an exact name in the table, the first matching glob in
    /// file order, a direct upstream model name (unless it is in the inbound protocol's own
    /// family), then the default entry (`*`).
    #[must_use]
    pub fn resolve(&self, name: &str, inbound: Protocol) -> Resolution {
        self.resolve_via(name, inbound).0
    }

    /// Like [`ModelTable::resolve`], also saying how the name resolved.
    #[must_use]
    pub fn resolve_via(&self, name: &str, inbound: Protocol) -> (Resolution, Option<Via>) {
        if let Some(e) = self
            .entries
            .iter()
            .find(|e| !is_glob(&e.pattern) && e.pattern == name)
        {
            return (Resolution::Entry(e.clone()), Some(Via::Exact));
        }
        if let Some(e) = self
            .entries
            .iter()
            .find(|e| e.pattern != "*" && is_glob(&e.pattern) && glob_match(&e.pattern, name))
        {
            return (
                Resolution::Entry(e.clone()),
                Some(Via::Glob(e.pattern.clone())),
            );
        }
        if !in_own_family(inbound, name) {
            if let Some(direct) = self.direct(name) {
                return (Resolution::Direct(direct), Some(Via::Direct));
            }
        }
        match self.entries.iter().find(|e| e.pattern == "*") {
            Some(e) => (Resolution::Entry(e.clone()), Some(Via::Default)),
            None => (Resolution::NotFound, None),
        }
    }

    fn direct(&self, name: &str) -> Option<Arc<ModelEntry>> {
        let (upstream, model) = match name.split_once('/') {
            Some((u, m)) if self.upstreams.contains_key(u) && !m.is_empty() => {
                (self.upstreams.get(u)?.clone(), m)
            }
            _ => (self.default_upstream.clone()?, name),
        };
        Some(Arc::new(ModelEntry {
            pattern: name.to_owned(),
            name: name.to_owned(),
            strategy: StrategyName::Fallback,
            targets: vec![RouteTarget {
                upstream,
                model: model.to_owned(),
                effort_levels: None,
                max_output_tokens: None,
                weight: 1,
                price: None,
            }],
        }))
    }
}

fn upstream_rt(
    u: &UpstreamConfig,
    config: &Config,
    resolver: &dyn SecretResolver,
) -> Result<(UpstreamRt, Option<String>), ConfigError> {
    let key_setting = format!("upstreams[{}].api_key", u.name);
    let (api_key, warning) = match &u.api_key {
        Some(r) => {
            let resolved = resolver
                .resolve(r)
                .map_err(|e| ConfigError::new(key_setting.clone(), e.to_string()))?;
            (
                Some(resolved.secret),
                resolved.warning.map(|w| format!("{key_setting}: {w}")),
            )
        }
        None => (None, None),
    };
    Ok((
        UpstreamRt {
            name: u.name.clone(),
            base_url: normalize_url(&u.url),
            protocol: u.protocol.into(),
            flavor: u.flavor.into(),
            api_key,
            timeouts: u.timeouts.clone(),
            effort_levels: u
                .effort_levels
                .as_ref()
                .map(|l| l.iter().map(|e| (*e).into()).collect()),
            think_tags: u.think_tags.unwrap_or(config.server.think_tags),
            key_setting,
            max_concurrent: u.max_concurrent,
            client: build_client(Duration::from_secs(u.timeouts.connect_secs)),
            slots: u
                .max_concurrent
                .map(|n| Arc::new(tokio::sync::Semaphore::new(n as usize))),
            health_check: u
                .health_check
                .as_ref()
                .map(|h| (h.path.clone(), Duration::from_secs(h.interval_secs))),
            rate: u.rate,
        },
        warning,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::paths::EnvFn;
    use crate::config::schema::{ModelConfig, ProtocolName, SecretRef, TargetConfig};
    use crate::secret::SystemResolver;

    fn up(name: &str) -> UpstreamConfig {
        UpstreamConfig {
            name: name.into(),
            url: format!("http://{name}.local/v1"),
            protocol: ProtocolName::Chat,
            flavor: crate::config::schema::FlavorName::Generic,
            api_key: None,
            max_concurrent: None,
            timeouts: Timeouts::default(),
            rate: None,
            health_check: None,
            effort_levels: None,
            think_tags: None,
            proxy: None,
        }
    }

    fn entry(pattern: &str, upstream: &str, model: &str) -> ModelConfig {
        ModelConfig {
            pattern: pattern.into(),
            name: None,
            strategy: StrategyName::Fallback,
            targets: vec![TargetConfig {
                upstream: upstream.into(),
                model: model.into(),
                weight: None,
                effort_levels: None,
                max_output_tokens: None,
                price: None,
            }],
        }
    }

    fn build(c: &Config) -> ModelTable {
        let env: EnvFn<'_> = &|_| None;
        ModelTable::build(c, &SystemResolver::new(env)).unwrap().0
    }

    fn cfg() -> Config {
        Config {
            upstreams: vec![up("fw"), up("local")],
            models: vec![
                entry("claude-opus-4", "fw", "exact"),
                entry("claude-opus-*", "fw", "opus-glob"),
                entry("claude-*", "local", "claude-glob"),
                entry("*", "local", "default"),
            ],
            ..Config::default()
        }
    }

    fn model_of(r: &Resolution) -> Option<String> {
        match r {
            Resolution::Entry(e) | Resolution::Direct(e) => Some(e.targets[0].model.clone()),
            Resolution::NotFound => None,
        }
    }

    #[test]
    fn an_exact_name_beats_globs_and_globs_resolve_in_file_order() {
        let t = build(&cfg());
        assert_eq!(
            model_of(&t.resolve("claude-opus-4", Protocol::Anthropic)).as_deref(),
            Some("exact")
        );
        assert_eq!(
            model_of(&t.resolve("claude-opus-4-5", Protocol::Anthropic)).as_deref(),
            Some("opus-glob")
        );
        assert_eq!(
            model_of(&t.resolve("claude-haiku-4", Protocol::Anthropic)).as_deref(),
            Some("claude-glob")
        );
    }

    #[test]
    fn the_default_entry_answers_what_nothing_else_does() {
        let t = build(&cfg());
        // In the client's own family the catch-all is the default entry.
        assert_eq!(
            model_of(&t.resolve("gpt-4.1", Protocol::Chat)).as_deref(),
            Some("default")
        );
        // Outside it, a direct upstream model name wins over the default.
        assert_eq!(
            model_of(&t.resolve("qwen3-coder", Protocol::Anthropic)).as_deref(),
            Some("qwen3-coder")
        );
    }

    #[test]
    fn a_direct_name_can_name_its_upstream() {
        let t = build(&cfg());
        match t.resolve("local/qwen3:8b", Protocol::Anthropic) {
            Resolution::Direct(e) => {
                assert_eq!(e.targets[0].upstream.name, "local");
                assert_eq!(e.targets[0].model, "qwen3:8b");
            }
            other => panic!("{other:?}"),
        }
        match t.resolve("accounts/fireworks/models/glm-5p1", Protocol::Anthropic) {
            Resolution::Direct(e) => {
                assert_eq!(
                    e.targets[0].upstream.name, "fw",
                    "an unknown prefix means the whole name is the model, on the default upstream"
                );
                assert_eq!(e.targets[0].model, "accounts/fireworks/models/glm-5p1");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_default_upstream_is_the_one_named_default_else_the_first() {
        let mut c = cfg();
        c.models.clear();
        assert!(
            matches!(build(&c).resolve("m", Protocol::Anthropic), Resolution::Direct(e) if e.targets[0].upstream.name == "fw")
        );
        c.upstreams.push(up("default"));
        assert!(
            matches!(build(&c).resolve("m", Protocol::Anthropic), Resolution::Direct(e) if e.targets[0].upstream.name == "default")
        );
    }

    #[test]
    fn a_family_name_with_no_match_and_no_default_is_not_found() {
        let mut c = cfg();
        c.models
            .retain(|m| m.pattern != "*" && m.pattern != "claude-*");
        let t = build(&c);
        assert!(matches!(
            t.resolve("claude-sonnet-9", Protocol::Anthropic),
            Resolution::NotFound
        ));
        assert!(matches!(
            t.resolve("gpt-9", Protocol::Chat),
            Resolution::NotFound
        ));
        // ...but a name from another family is taken as a direct upstream model.
        assert!(matches!(
            t.resolve("llama3", Protocol::Anthropic),
            Resolution::Direct(_)
        ));
    }

    #[test]
    fn with_no_upstreams_nothing_resolves_directly() {
        let t = build(&Config::default());
        assert!(matches!(
            t.resolve("anything", Protocol::Anthropic),
            Resolution::NotFound
        ));
    }

    #[test]
    fn the_family_of_each_inbound_protocol() {
        assert!(
            in_own_family(Protocol::Anthropic, "claude-opus-4")
                && in_own_family(Protocol::Anthropic, "claude")
        );
        assert!(!in_own_family(Protocol::Anthropic, "gpt-4"));
        for n in ["gpt-4.1", "chatgpt-4o-latest", "o3-mini"] {
            assert!(
                in_own_family(Protocol::Chat, n) && in_own_family(Protocol::Responses, n),
                "{n}"
            );
        }
        assert!(!in_own_family(Protocol::Chat, "claude-x"));
        assert!(
            in_own_family(Protocol::Gemini, "gemini-2.5-pro")
                && !in_own_family(Protocol::Bedrock, "x")
        );
    }

    #[test]
    fn upstream_urls_are_normalised_and_targets_become_core_targets() {
        let t = build(&cfg());
        let up = t.upstream("fw").unwrap();
        assert_eq!(up.base_url, "http://fw.local");
        assert_eq!(up.connect_timeout(), Duration::from_secs(10));
        let rt = &t.entry_by_name("claude-opus-4").unwrap().targets[0];
        assert_eq!(rt.label(), "fw/exact");
        let core = rt.core_target(true);
        assert_eq!(
            (core.upstream.as_str(), core.protocol, core.model.as_str()),
            ("fw", Protocol::Chat, "exact")
        );
        assert!(core.think_tags);
        assert!(
            !rt.core_target(false).think_tags,
            "server.think_tags: false turns it off everywhere"
        );
        assert_eq!(t.upstreams().count(), 2);
        assert_eq!(t.entries().len(), 4);
    }

    #[test]
    fn secrets_are_resolved_at_build_and_failures_name_the_setting() {
        let mut c = cfg();
        c.upstreams[0].api_key = Some(SecretRef::Env("FW_KEY".into()));
        let with_key: EnvFn<'_> = &|n| (n == "FW_KEY").then(|| "secret-value".to_owned());
        let (t, warnings) = ModelTable::build(&c, &SystemResolver::new(with_key)).unwrap();
        assert_eq!(
            t.upstream("fw").unwrap().api_key.as_ref().unwrap().expose(),
            "secret-value"
        );
        assert!(warnings.is_empty());
        assert!(!format!("{:?}", t.upstream("fw").unwrap()).contains("secret-value"));
        let without: EnvFn<'_> = &|_| None;
        let errs = ModelTable::build(&c, &SystemResolver::new(without)).unwrap_err();
        assert_eq!(errs[0].path, "upstreams[fw].api_key");
        assert!(errs[0].message.contains("FW_KEY"));
    }

    #[test]
    fn a_literal_secret_produces_a_warning() {
        let mut c = cfg();
        c.upstreams[0].api_key = Some(SecretRef::Value(crate::config::schema::Redacted(
            "lit".into(),
        )));
        let env: EnvFn<'_> = &|_| None;
        let (_, warnings) = ModelTable::build(&c, &SystemResolver::new(env)).unwrap();
        assert!(warnings[0].contains("upstreams[fw].api_key") && warnings[0].contains("literal"));
    }

    #[test]
    fn per_target_overrides_flow_through() {
        let mut c = cfg();
        c.upstreams[0].effort_levels = Some(vec![crate::config::schema::EffortName::Low]);
        c.models[0].targets[0].max_output_tokens = Some(77);
        c.models[0].targets[0].weight = Some(3);
        let t = build(&c);
        let rt = &t.entry_by_name("claude-opus-4").unwrap().targets[0];
        assert_eq!((rt.max_output_tokens, rt.weight), (Some(77), 3));
        assert_eq!(rt.core_target(true).effort_levels, Some(vec![Effort::Low]));
    }
}
