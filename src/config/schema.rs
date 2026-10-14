//! The configuration schema.
//!
//! The file is YAML. Unknown keys are errors (`deny_unknown_fields` everywhere), so a typo never
//! silently turns a setting off. Secrets are never in the file: a secret is a reference
//! ([`SecretRef`]) to an environment variable, a file, or a credential helper.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use airmate_core::flavor::Flavor;
use airmate_core::ir::{Effort, Protocol};
use airmate_core::policy::UnknownFields;

/// The default port.
pub const DEFAULT_PORT: u16 = 9997;
/// The default bind address.
pub const DEFAULT_BIND: &str = "127.0.0.1";

fn default_bind() -> String {
    DEFAULT_BIND.to_owned()
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_max_body() -> u64 {
    64 * 1024 * 1024
}
fn default_max_concurrent() -> u32 {
    16
}
fn default_buffer_cap() -> u64 {
    4 * 1024 * 1024
}
fn yes() -> bool {
    true
}
fn default_connect() -> u64 {
    10
}
fn default_first_byte() -> u64 {
    60
}
fn default_idle() -> u64 {
    300
}
fn default_failure_threshold() -> u32 {
    5
}
fn default_cooldown() -> u64 {
    30
}
fn default_max_attempts() -> u32 {
    4
}
fn default_total_secs() -> u64 {
    120
}

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// The listener and request-handling settings.
    pub server: ServerConfig,
    /// The upstreams.
    pub upstreams: Vec<UpstreamConfig>,
    /// The model entries clients name.
    pub models: Vec<ModelConfig>,
    /// Routing rules, first match wins.
    pub rules: Vec<RuleConfig>,
    /// Authenticated client identities.
    pub clients: Vec<ClientConfig>,
    /// Overrides for the built-in capability catalog.
    pub catalog: Vec<CatalogOverride>,
    /// External classifiers that rules can consult.
    pub classifiers: Vec<ClassifierConfig>,
    /// Circuit breaker defaults.
    pub circuit: CircuitConfig,
    /// Retry and failover limits.
    pub failover: FailoverConfig,
    /// The usage ledger.
    pub usage: UsageConfig,
    /// Logging.
    pub log: LogConfig,
    /// Named overrides, applied over the rest with `--profile <name>`.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub profiles: BTreeMap<String, serde_norway::Value>,
}

/// `server:` the listener and request handling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// The address to bind. Loopback only by default.
    pub bind: String,
    /// The port. `0` picks a free one. Defaults to 9997.
    pub port: u16,
    /// A Unix-domain socket to listen on instead of TCP.
    pub unix_socket: Option<String>,
    /// The largest request body accepted, in bytes.
    pub max_body_bytes: u64,
    /// Concurrent upstream calls, across all upstreams.
    pub max_concurrent: u32,
    /// How long a request may wait for a concurrency slot; unset answers 429 at once.
    pub queue_timeout_secs: Option<u64>,
    /// What to do with request fields the policy table does not know.
    pub unknown_fields: UnknownFieldsMode,
    /// Whether a leading `<think>` block in content is split out as reasoning.
    pub think_tags: bool,
    /// The cap on the stream interleaving buffer, in bytes.
    pub buffer_cap_bytes: u64,
    /// Seconds of upstream silence before a keep-alive `ping` is sent to the client.
    pub keepalive_secs: u64,
    /// Request headers that rules may match on; no other header is ever visible to a rule.
    pub rule_headers: Vec<String>,
    /// Keep a conversation on the target that served its previous turn.
    pub affinity: bool,
    /// `Host` values accepted besides loopback; required for a non-loopback bind.
    pub allowed_hosts: Vec<String>,
    /// Acknowledges the risk of a non-loopback bind without TLS.
    pub insecure_remote: bool,
    /// TLS material, for the `tls` feature.
    pub tls: Option<TlsConfig>,
    /// Seconds shutdown lets in-flight requests finish before ending their streams.
    pub shutdown_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            unix_socket: None,
            max_body_bytes: default_max_body(),
            max_concurrent: default_max_concurrent(),
            queue_timeout_secs: None,
            unknown_fields: UnknownFieldsMode::Reject,
            think_tags: yes(),
            buffer_cap_bytes: default_buffer_cap(),
            keepalive_secs: 15,
            rule_headers: Vec::new(),
            affinity: true,
            allowed_hosts: Vec::new(),
            insecure_remote: false,
            tls: None,
            shutdown_timeout_secs: 30,
        }
    }
}

/// TLS certificate and key paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// The certificate chain, PEM.
    pub cert: String,
    /// The private key, PEM.
    pub key: String,
}

/// `unknown_fields:` `reject` or `warn`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum UnknownFieldsMode {
    /// Refuse requests with unknown fields.
    #[default]
    Reject,
    /// Ignore unknown fields and report them.
    Warn,
}

impl From<UnknownFieldsMode> for UnknownFields {
    fn from(m: UnknownFieldsMode) -> Self {
        match m {
            UnknownFieldsMode::Reject => UnknownFields::Reject,
            UnknownFieldsMode::Warn => UnknownFields::Warn,
        }
    }
}

/// An upstream protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolName {
    /// OpenAI Chat Completions.
    Chat,
    /// OpenAI Responses.
    Responses,
    /// Anthropic Messages.
    Anthropic,
    /// Google Gemini `generateContent`.
    Gemini,
    /// AWS Bedrock Converse.
    Bedrock,
}

impl From<ProtocolName> for Protocol {
    fn from(p: ProtocolName) -> Self {
        match p {
            ProtocolName::Chat => Protocol::Chat,
            ProtocolName::Responses => Protocol::Responses,
            ProtocolName::Anthropic => Protocol::Anthropic,
            ProtocolName::Gemini => Protocol::Gemini,
            ProtocolName::Bedrock => Protocol::Bedrock,
        }
    }
}

/// A provider flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum FlavorName {
    /// Any OpenAI-compatible server.
    #[default]
    Generic,
    /// OpenAI.
    Openai,
    /// Fireworks AI.
    Fireworks,
}

impl From<FlavorName> for Flavor {
    fn from(f: FlavorName) -> Self {
        match f {
            FlavorName::Generic => Flavor::Generic,
            FlavorName::Openai => Flavor::OpenAi,
            FlavorName::Fireworks => Flavor::Fireworks,
        }
    }
}

/// A reasoning effort level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffortName {
    /// `low`
    Low,
    /// `medium`
    Medium,
    /// `high`
    High,
    /// `xhigh`
    Xhigh,
    /// `max`
    Max,
}

impl From<EffortName> for Effort {
    fn from(e: EffortName) -> Self {
        match e {
            EffortName::Low => Effort::Low,
            EffortName::Medium => Effort::Medium,
            EffortName::High => Effort::High,
            EffortName::Xhigh => Effort::XHigh,
            EffortName::Max => Effort::Max,
        }
    }
}

/// A reference to a secret. The file never contains one.
///
/// In YAML it is a one-key mapping: `{ env: NAME }`, `{ file: /path }`,
/// `{ command: [helper, arg] }`, `{ keychain: service }` or `{ value: literal }`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SecretRefRaw", into = "SecretRefRaw")]
pub enum SecretRef {
    /// An environment variable.
    Env(String),
    /// A file; refused unless it is readable by its owner only.
    File(String),
    /// A credential helper whose stdout is the secret, run without a shell.
    Command(Vec<String>),
    /// The OS keychain.
    Keychain(String),
    /// A literal. Accepted with a loud warning; never printed.
    Value(Redacted),
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct SecretRefRaw {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    keychain: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    value: Option<Redacted>,
}

impl TryFrom<SecretRefRaw> for SecretRef {
    type Error = String;

    fn try_from(r: SecretRefRaw) -> Result<Self, String> {
        let mut found = Vec::new();
        if let Some(v) = r.env {
            found.push(Self::Env(v));
        }
        if let Some(v) = r.file {
            found.push(Self::File(v));
        }
        if let Some(v) = r.command {
            found.push(Self::Command(v));
        }
        if let Some(v) = r.keychain {
            found.push(Self::Keychain(v));
        }
        if let Some(v) = r.value {
            found.push(Self::Value(v));
        }
        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(
                "a secret needs one of `env`, `file`, `command`, `keychain` or `value`".to_owned(),
            ),
            _ => Err(
                "a secret takes exactly one of `env`, `file`, `command`, `keychain` or `value`"
                    .to_owned(),
            ),
        }
    }
}

impl From<SecretRef> for SecretRefRaw {
    fn from(s: SecretRef) -> Self {
        let mut r = Self::default();
        match s {
            SecretRef::Env(v) => r.env = Some(v),
            SecretRef::File(v) => r.file = Some(v),
            SecretRef::Command(v) => r.command = Some(v),
            SecretRef::Keychain(v) => r.keychain = Some(v),
            SecretRef::Value(v) => r.value = Some(v),
        }
        r
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(n) => write!(f, "Env({n})"),
            Self::File(p) => write!(f, "File({p})"),
            Self::Command(c) => write!(f, "Command({c:?})"),
            Self::Keychain(s) => write!(f, "Keychain({s})"),
            Self::Value(_) => f.write_str("Value(<redacted>)"),
        }
    }
}

/// A string that is never printed or serialised.
#[derive(Clone, PartialEq, Eq)]
pub struct Redacted(pub String);

impl fmt::Debug for Redacted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Serialize for Redacted {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("<redacted>")
    }
}

impl<'de> Deserialize<'de> for Redacted {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d).map(Redacted)
    }
}

/// Upstream timeouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Timeouts {
    /// Connecting.
    pub connect_secs: u64,
    /// Waiting for the first byte.
    pub first_byte_secs: u64,
    /// Idle time between reads once the response started.
    pub idle_secs: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect_secs: default_connect(),
            first_byte_secs: default_first_byte(),
            idle_secs: default_idle(),
        }
    }
}

/// Request and token rate limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Requests per minute.
    pub rpm: Option<u32>,
    /// Input tokens per minute.
    pub tpm: Option<u64>,
}

/// An active health probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCheck {
    /// The path to GET.
    pub path: String,
    /// Seconds between probes.
    pub interval_secs: u64,
}

/// `upstreams:` one endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// The name other settings refer to.
    pub name: String,
    /// The base URL. A trailing `/v1` is dropped; airmate appends `/v1/...`.
    pub url: String,
    /// The protocol the upstream speaks.
    #[serde(default = "default_protocol")]
    pub protocol: ProtocolName,
    /// The provider flavor.
    #[serde(default)]
    pub flavor: FlavorName,
    /// The upstream's API key. Optional for local servers.
    #[serde(default)]
    pub api_key: Option<SecretRef>,
    /// Concurrent calls to this upstream.
    #[serde(default)]
    pub max_concurrent: Option<u32>,
    /// Timeouts.
    #[serde(default)]
    pub timeouts: Timeouts,
    /// Rate limits for this upstream.
    #[serde(default)]
    pub rate: Option<RateLimit>,
    /// An active health probe.
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    /// Overrides the effort levels the flavor accepts.
    #[serde(default)]
    pub effort_levels: Option<Vec<EffortName>>,
    /// Overrides `server.think_tags` for this upstream.
    #[serde(default)]
    pub think_tags: Option<bool>,
    /// An HTTP proxy for this upstream.
    #[serde(default)]
    pub proxy: Option<String>,
    /// AWS credentials, for `protocol: bedrock` (a Bedrock API key goes in `api_key` instead).
    #[serde(default)]
    pub aws: Option<AwsConfig>,
}

/// `upstreams[].aws:` credentials for signing Bedrock requests with Signature Version 4.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwsConfig {
    /// The region, e.g. `us-east-1`. Defaults to the one in a `bedrock-runtime.<region>` URL.
    #[serde(default)]
    pub region: Option<String>,
    /// The access key id.
    pub access_key_id: SecretRef,
    /// The secret access key.
    pub secret_access_key: SecretRef,
    /// The session token, for temporary credentials.
    #[serde(default)]
    pub session_token: Option<SecretRef>,
}

fn default_protocol() -> ProtocolName {
    ProtocolName::Chat
}

/// A price in US dollars per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// Input tokens.
    pub input_per_mtok: f64,
    /// Output tokens.
    pub output_per_mtok: f64,
    /// Cached input tokens, if priced differently.
    #[serde(default)]
    pub cache_read_per_mtok: Option<f64>,
}

/// One (upstream, model) target of a model entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// The upstream's name.
    pub upstream: String,
    /// The model name to send upstream.
    pub model: String,
    /// The weight, for the `weighted` strategy.
    #[serde(default)]
    pub weight: Option<u32>,
    /// Overrides the accepted effort levels.
    #[serde(default)]
    pub effort_levels: Option<Vec<EffortName>>,
    /// Caps the output tokens sent upstream.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Overrides the catalog price.
    #[serde(default)]
    pub price: Option<Price>,
}

/// How targets of a model entry are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StrategyName {
    /// In order.
    #[default]
    Fallback,
    /// Weighted random among healthy targets.
    Weighted,
    /// Rotate.
    RoundRobin,
    /// The lowest time to first byte.
    LeastLatency,
    /// The cheapest by price.
    LeastCost,
}

/// `models:` what a client names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    /// A glob (`*` matches any run of characters) or an exact name.
    #[serde(rename = "match")]
    pub pattern: String,
    /// A name rules and clients can refer to; defaults to the pattern.
    #[serde(default)]
    pub name: Option<String>,
    /// How targets are ordered.
    #[serde(default)]
    pub strategy: StrategyName,
    /// The targets.
    pub targets: Vec<TargetConfig>,
}

impl ModelConfig {
    /// The name rules use to refer to this entry.
    #[must_use]
    pub fn entry_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.pattern)
    }
}

/// The inbound protocols a rule can match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InboundName {
    /// Anthropic Messages.
    Anthropic,
    /// OpenAI Chat Completions.
    Chat,
    /// OpenAI Responses.
    Responses,
}

/// A range of hours, `from` inclusive to `to` exclusive, wrapping past midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HourRange {
    /// Start hour, 0 to 23.
    pub from: u8,
    /// End hour, 0 to 24.
    pub to: u8,
}

/// A rule's predicate. Every field is optional and they are ANDed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RuleWhen {
    /// A glob over the model the client named.
    #[serde(default)]
    pub model: Option<String>,
    /// The client name.
    #[serde(default)]
    pub client: Option<String>,
    /// The inbound protocol.
    #[serde(default)]
    pub inbound: Option<InboundName>,
    /// Whether the request streams.
    #[serde(default)]
    pub stream: Option<bool>,
    /// Whether tools are present.
    #[serde(default)]
    pub tools: Option<bool>,
    /// Whether images are present.
    #[serde(default)]
    pub images: Option<bool>,
    /// Whether thinking was requested.
    #[serde(default)]
    pub thinking: Option<bool>,
    /// The lowest effort.
    #[serde(default)]
    pub effort_min: Option<EffortName>,
    /// The highest effort.
    #[serde(default)]
    pub effort_max: Option<EffortName>,
    /// The fewest estimated input tokens.
    #[serde(default)]
    pub est_input_tokens_min: Option<u64>,
    /// The most estimated input tokens.
    #[serde(default)]
    pub est_input_tokens_max: Option<u64>,
    /// The smallest `max_tokens`.
    #[serde(default)]
    pub max_tokens_min: Option<u32>,
    /// The largest `max_tokens`.
    #[serde(default)]
    pub max_tokens_max: Option<u32>,
    /// Header name to value glob; only allow-listed headers are visible.
    #[serde(default)]
    pub header: BTreeMap<String, String>,
    /// The hours of the day the rule applies.
    #[serde(default)]
    pub hour_of_day: Option<HourRange>,
    /// A classifier label that must match.
    #[serde(default)]
    pub classifier: Option<ClassifierMatch>,
}

/// A classifier label a rule requires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierMatch {
    /// The classifier's name.
    pub name: String,
    /// The label that must come back.
    pub label: String,
}

/// `rules:` a predicate plus an action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    /// A name for logs and the `x-airmate-rule` header.
    pub name: String,
    /// The predicate.
    #[serde(default)]
    pub when: RuleWhen,
    /// A model entry to use.
    #[serde(default, rename = "use")]
    pub use_entry: Option<String>,
    /// Or an inline target list.
    #[serde(default)]
    pub targets: Option<Vec<TargetConfig>>,
}

/// A spend cap per period, in US dollars.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Per day.
    #[serde(default)]
    pub usd_per_day: Option<f64>,
    /// Per week.
    #[serde(default)]
    pub usd_per_week: Option<f64>,
    /// Per month.
    #[serde(default)]
    pub usd_per_month: Option<f64>,
}

/// `clients:` an authenticated identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// The name attached to logs, metrics and rule predicates.
    pub name: String,
    /// `sha256:<hex>` of the key; `airmate keys add` writes this.
    pub key_hash: String,
    /// Allowed model entries, as globs. Empty means all.
    #[serde(default)]
    pub models: Vec<String>,
    /// Rate limits for this client.
    #[serde(default)]
    pub rate: Option<RateLimit>,
    /// Spend caps.
    #[serde(default)]
    pub budget: Option<Budget>,
    /// A model entry to fall back to when the budget is exhausted, instead of answering 429.
    #[serde(default)]
    pub budget_fallback: Option<String>,
}

/// What a classifier may see of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassifierInputField {
    /// The text of the last user message.
    LastUserMessage,
    /// The system prompt.
    System,
    /// A short digest: message and tool counts and the estimated size.
    Summary,
}

fn default_classifier_timeout() -> u64 {
    300
}

/// `classifiers:` an external HTTP endpoint that labels a request, for example by complexity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassifierConfig {
    /// The name rules refer to.
    pub name: String,
    /// The endpoint. It receives `{"input": {...}}` and answers `{"label": "..."}`.
    pub url: String,
    /// How long to wait, in milliseconds. A slow or failing classifier gives no label.
    #[serde(default = "default_classifier_timeout")]
    pub timeout_ms: u64,
    /// What the classifier is allowed to see. Never the upstream key.
    #[serde(default)]
    pub input: Vec<ClassifierInputField>,
}

/// `catalog:` an override of what a model can do. Only the fields set are overridden.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CatalogOverride {
    /// A glob over upstream model names.
    #[serde(rename = "match")]
    pub pattern: String,
    /// The context window, in tokens.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// The most output tokens the model can produce.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Tool calling.
    #[serde(default)]
    pub tools: Option<bool>,
    /// Several tool calls in one turn.
    #[serde(default)]
    pub parallel_tools: Option<bool>,
    /// Image input.
    #[serde(default)]
    pub vision: Option<bool>,
    /// Document input.
    #[serde(default)]
    pub documents: Option<bool>,
    /// Reasoning.
    #[serde(default)]
    pub thinking: Option<bool>,
    /// A reasoning effort control.
    #[serde(default)]
    pub effort: Option<bool>,
    /// Structured (JSON schema) output.
    #[serde(default)]
    pub structured_output: Option<bool>,
    /// Streaming.
    #[serde(default)]
    pub streaming: Option<bool>,
    /// List prices.
    #[serde(default)]
    pub price: Option<Price>,
}

/// `circuit:` breaker defaults.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CircuitConfig {
    /// Consecutive retryable failures that open the breaker.
    pub failure_threshold: u32,
    /// Seconds the breaker stays open.
    pub cooldown_secs: u64,
    /// The longest the cooldown grows to after repeated failed probes, in seconds.
    pub max_cooldown_secs: u64,
    /// How many recent outcomes the error rate is computed over.
    pub window: usize,
    /// The fewest outcomes in the window before the error rate can open the breaker.
    pub min_samples: usize,
    /// The error rate over the window that opens the breaker (0.0 to 1.0).
    pub error_rate: f64,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            cooldown_secs: default_cooldown(),
            max_cooldown_secs: 600,
            window: 20,
            min_samples: 10,
            error_rate: 0.5,
        }
    }
}

/// `failover:` retry and failover limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FailoverConfig {
    /// Total attempts per request.
    pub max_attempts: u32,
    /// Wall-clock budget before the first byte, in seconds.
    pub total_secs: u64,
    /// Retries on the same target before moving on.
    pub retries: u32,
    /// Backoff base, in milliseconds.
    pub backoff_base_ms: u64,
    /// Backoff cap, in milliseconds.
    pub backoff_cap_ms: u64,
    /// The longest `Retry-After` waited for; longer marks the target cooling down.
    pub retry_after_ceiling_secs: u64,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            total_secs: default_total_secs(),
            retries: 0,
            backoff_base_ms: 250,
            backoff_cap_ms: 5_000,
            retry_after_ceiling_secs: 10,
        }
    }
}

/// `usage:` the usage ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UsageConfig {
    /// Whether to write the ledger.
    pub ledger: bool,
    /// `builtin`, or the path of a price override file.
    pub prices: String,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            ledger: false,
            prices: "builtin".to_owned(),
        }
    }
}

/// Log rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rotate {
    /// Rotate when the file reaches this size.
    pub max_size_mb: u64,
    /// How many rotated files to keep.
    pub keep: u32,
}

/// `log:` logging.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogConfig {
    /// Whether to write `requests.jsonl`.
    pub requests: bool,
    /// Whether to log request and response bodies, to a separate file.
    pub bodies: bool,
    /// Body log size cap per body, in bytes.
    pub body_max_bytes: u64,
    /// Body log retention, in days.
    pub body_retention_days: u32,
    /// Rotation for the request log.
    pub rotate: Option<Rotate>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            requests: true,
            bodies: false,
            body_max_bytes: 64 * 1024,
            body_retention_days: 7,
            rotate: Some(Rotate {
                max_size_mb: 64,
                keep: 5,
            }),
        }
    }
}
