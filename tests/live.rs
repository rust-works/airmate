//! Live conformance: `airmate doctor`'s probes against real providers.
//!
//! These never run unless asked for. Set `AIRMATE_LIVE=1` and the variables for the provider you
//! want to check; a provider whose variables are missing is skipped with a message saying which.
//! A provider quirk in the docs is "verified" only once its test here has passed.
//!
//! | Provider | Variables (required unless a default is shown) |
//! |---|---|
//! | OpenAI Chat and Responses | `AIRMATE_LIVE_OPENAI_KEY`, `AIRMATE_LIVE_OPENAI_MODEL` (`gpt-4.1-mini`) |
//! | Anthropic | `AIRMATE_LIVE_ANTHROPIC_KEY`, `AIRMATE_LIVE_ANTHROPIC_MODEL` (`claude-haiku-4-5`) |
//! | Fireworks, Chat and Anthropic modes | `AIRMATE_LIVE_FIREWORKS_KEY`, `AIRMATE_LIVE_FIREWORKS_MODEL` |
//! | Ollama | `AIRMATE_LIVE_OLLAMA_MODEL`, `AIRMATE_LIVE_OLLAMA_URL` (`http://127.0.0.1:11434`) |
//! | vLLM, LM Studio or any OpenAI-compatible server | `AIRMATE_LIVE_COMPAT_URL`, `AIRMATE_LIVE_COMPAT_MODEL`, `AIRMATE_LIVE_COMPAT_KEY` (optional) |
//! | Gemini | `AIRMATE_LIVE_GEMINI_KEY`, `AIRMATE_LIVE_GEMINI_MODEL` (`gemini-2.5-flash`) |
//! | Bedrock | `AIRMATE_LIVE_BEDROCK_MODEL`, `AIRMATE_LIVE_BEDROCK_REGION` (`us-east-1`), plus `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` (and `AWS_SESSION_TOKEN`) |

use std::sync::Arc;

use airmate::config::load_str;
use airmate::doctor::{render, run_target};
use airmate::secret::{Secret, SystemResolver};
use airmate::server::Tables;

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn enabled() -> bool {
    var("AIRMATE_LIVE").as_deref() == Some("1")
}

/// Values for the variables, or `None` (with a note) when the provider is not configured.
fn require(provider: &str, names: &[&str]) -> Option<Vec<String>> {
    if !enabled() {
        eprintln!("skipped {provider}: set AIRMATE_LIVE=1 to run live conformance tests");
        return None;
    }
    let values: Option<Vec<String>> = names.iter().map(|n| var(n)).collect();
    if values.is_none() {
        eprintln!("skipped {provider}: set {}", names.join(", "));
    }
    values
}

// The upstream definition for each provider, shared by the live tests and the offline check that
// they are valid configuration.
fn openai(protocol: &str) -> String {
    format!("{{ name: live, url: \"https://api.openai.com\", protocol: {protocol}, flavor: openai, api_key: {{ env: AIRMATE_LIVE_OPENAI_KEY }} }}")
}

fn anthropic_up() -> String {
    "{ name: live, url: \"https://api.anthropic.com\", protocol: anthropic, api_key: { env: AIRMATE_LIVE_ANTHROPIC_KEY } }".into()
}

fn fireworks(protocol: &str) -> String {
    format!("{{ name: live, url: \"https://api.fireworks.ai/inference\", protocol: {protocol}, flavor: fireworks, api_key: {{ env: AIRMATE_LIVE_FIREWORKS_KEY }} }}")
}

fn ollama_up(url: &str) -> String {
    format!("{{ name: live, url: \"{url}\", protocol: chat }}")
}

fn compat(url: &str, key: bool) -> String {
    let key = if key {
        ", api_key: { env: AIRMATE_LIVE_COMPAT_KEY }"
    } else {
        ""
    };
    format!("{{ name: live, url: \"{url}\", protocol: chat{key} }}")
}

fn gemini_up() -> String {
    "{ name: live, url: \"https://generativelanguage.googleapis.com\", protocol: gemini, api_key: { env: AIRMATE_LIVE_GEMINI_KEY } }".into()
}

fn bedrock_up(region: &str, session: bool) -> String {
    let session = if session {
        ", session_token: { env: AWS_SESSION_TOKEN }"
    } else {
        ""
    };
    format!("{{ name: live, url: \"https://bedrock-runtime.{region}.amazonaws.com\", protocol: bedrock, aws: {{ region: {region}, access_key_id: {{ env: AWS_ACCESS_KEY_ID }}, secret_access_key: {{ env: AWS_SECRET_ACCESS_KEY }}{session} }} }}")
}

fn config_for(upstream: &str, model: &str) -> String {
    format!("upstreams:\n  - {upstream}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: live, model: \"{model}\" }} ] }}\n")
}

/// The definitions are valid configuration: a typo in one would otherwise only show when someone
/// with a key ran it.
#[test]
fn the_provider_definitions_are_valid_configuration() {
    let env = |n: &str| Some(format!("dummy-{n}"));
    for up in [
        openai("chat"),
        openai("responses"),
        anthropic_up(),
        fireworks("chat"),
        fireworks("anthropic"),
        ollama_up("http://127.0.0.1:11434"),
        compat("http://127.0.0.1:8000", true),
        compat("http://127.0.0.1:1234", false),
        gemini_up(),
        bedrock_up("us-east-1", false),
        bedrock_up("eu-west-1", true),
    ] {
        let cfg =
            Arc::new(load_str(&config_for(&up, "m"), None).unwrap_or_else(|e| panic!("{up}: {e}")));
        Tables::build(cfg, &SystemResolver::new(&env), &Secret::new("t"))
            .unwrap_or_else(|e| panic!("{up}: {e:?}"));
    }
}

async fn check(provider: &str, upstream_yaml: &str, model: &str) {
    let yaml = config_for(upstream_yaml, model);
    let cfg =
        Arc::new(load_str(&yaml, None).unwrap_or_else(|e| panic!("{provider}: bad config: {e}")));
    let (tables, _) = Tables::build(
        cfg,
        &SystemResolver::new(&|n| std::env::var(n).ok()),
        &Secret::new("live"),
    )
    .unwrap_or_else(|e| panic!("{provider}: {e:?}"));
    let target = tables.models.entries()[0].targets[0].clone();
    let report = run_target(&target, &tables.catalog.for_target(&target)).await;
    let text = render(std::slice::from_ref(&report));
    println!("{text}");
    assert!(
        !report.failed(),
        "{provider} failed live conformance:\n{text}"
    );
}

#[tokio::test]
async fn openai_chat() {
    let Some(_) = require("openai chat", &["AIRMATE_LIVE_OPENAI_KEY"]) else {
        return;
    };
    let model = var("AIRMATE_LIVE_OPENAI_MODEL").unwrap_or_else(|| "gpt-4.1-mini".into());
    check("openai chat", &openai("chat"), &model).await;
}

#[tokio::test]
async fn openai_responses() {
    let Some(_) = require("openai responses", &["AIRMATE_LIVE_OPENAI_KEY"]) else {
        return;
    };
    let model = var("AIRMATE_LIVE_OPENAI_MODEL").unwrap_or_else(|| "gpt-4.1-mini".into());
    check("openai responses", &openai("responses"), &model).await;
}

#[tokio::test]
async fn anthropic() {
    let Some(_) = require("anthropic", &["AIRMATE_LIVE_ANTHROPIC_KEY"]) else {
        return;
    };
    let model = var("AIRMATE_LIVE_ANTHROPIC_MODEL").unwrap_or_else(|| "claude-haiku-4-5".into());
    check("anthropic", &anthropic_up(), &model).await;
}

#[tokio::test]
async fn fireworks_in_chat_mode() {
    let Some(v) = require(
        "fireworks chat",
        &["AIRMATE_LIVE_FIREWORKS_KEY", "AIRMATE_LIVE_FIREWORKS_MODEL"],
    ) else {
        return;
    };
    check("fireworks chat", &fireworks("chat"), &v[1]).await;
}

#[tokio::test]
async fn fireworks_in_anthropic_mode() {
    let Some(v) = require(
        "fireworks anthropic",
        &["AIRMATE_LIVE_FIREWORKS_KEY", "AIRMATE_LIVE_FIREWORKS_MODEL"],
    ) else {
        return;
    };
    check("fireworks anthropic", &fireworks("anthropic"), &v[1]).await;
}

#[tokio::test]
async fn ollama() {
    let Some(v) = require("ollama", &["AIRMATE_LIVE_OLLAMA_MODEL"]) else {
        return;
    };
    let url = var("AIRMATE_LIVE_OLLAMA_URL").unwrap_or_else(|| "http://127.0.0.1:11434".into());
    check("ollama", &ollama_up(&url), &v[0]).await;
}

#[tokio::test]
async fn an_openai_compatible_server() {
    let Some(v) = require(
        "openai-compatible",
        &["AIRMATE_LIVE_COMPAT_URL", "AIRMATE_LIVE_COMPAT_MODEL"],
    ) else {
        return;
    };
    check(
        "openai-compatible",
        &compat(&v[0], var("AIRMATE_LIVE_COMPAT_KEY").is_some()),
        &v[1],
    )
    .await;
}

#[tokio::test]
async fn gemini() {
    let Some(_) = require("gemini", &["AIRMATE_LIVE_GEMINI_KEY"]) else {
        return;
    };
    let model = var("AIRMATE_LIVE_GEMINI_MODEL").unwrap_or_else(|| "gemini-2.5-flash".into());
    check("gemini", &gemini_up(), &model).await;
}

#[tokio::test]
async fn bedrock() {
    let Some(v) = require(
        "bedrock",
        &[
            "AIRMATE_LIVE_BEDROCK_MODEL",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
        ],
    ) else {
        return;
    };
    let region = var("AIRMATE_LIVE_BEDROCK_REGION").unwrap_or_else(|| "us-east-1".into());
    check(
        "bedrock",
        &bedrock_up(&region, var("AWS_SESSION_TOKEN").is_some()),
        &v[0],
    )
    .await;
}
