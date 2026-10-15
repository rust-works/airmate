//! Planning a request's route: which entry, which targets, in what order, and what was skipped.
//!
//! In order: the first matching rule selects a model entry (or carries its own targets); else the
//! model name resolves through the table. Targets that lack a capability the request needs are
//! skipped and the skip recorded; the rest are ordered by the entry's strategy, with the target
//! that served the conversation's previous turn preferred. State that changes while a request is
//! in flight (an open breaker, a rate limit) is checked at attempt time, not here.

use std::collections::HashMap;
use std::sync::Arc;

use airmate_core::error::{ErrorKind, GatewayError};
use airmate_core::facts::Facts;
use airmate_core::ir::Protocol;

use super::catalog::{filter, Capabilities, Catalog, Skip, SkipReason};
use super::rules::{Action, CompiledRule, RuleCtx, RuleSet};
use super::state::{RouterState, TargetState, UpstreamHealth};
use super::strategy::{order, prefer, Candidate};
use super::table::{ModelEntry, ModelTable, Resolution, RouteTarget, Via};
use crate::config::schema::{Config, StrategyName};

/// A target to try, with what the attempt loop needs.
#[derive(Clone)]
pub struct Planned {
    /// The target.
    pub target: RouteTarget,
    /// What has been learned about it.
    pub state: Arc<TargetState>,
    /// The health of its upstream.
    pub health: Arc<UpstreamHealth>,
    /// What it can do.
    pub caps: Capabilities,
}

impl std::fmt::Debug for Planned {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Planned")
            .field("target", &self.target.label())
            .finish_non_exhaustive()
    }
}

/// The route a request will take.
#[derive(Debug, Clone)]
pub struct Route {
    /// The model entry that was chosen.
    pub entry_name: String,
    /// The rule that chose it, if any.
    pub rule: Option<String>,
    /// How the model name resolved when no rule chose.
    pub via: Option<Via>,
    /// How the targets were ordered.
    pub strategy: StrategyName,
    /// The targets to try, in order.
    pub targets: Vec<Planned>,
    /// The targets skipped for what the request needs.
    pub skipped: Vec<Skip>,
    /// The conversation key affinity used, if any.
    pub affinity_key: Option<String>,
}

/// What the router needs from the request.
pub struct PlanInput<'a> {
    /// The routing facts.
    pub facts: &'a Facts,
    /// The authenticated client.
    pub client: &'a str,
    /// The raw body, for affinity and classifiers.
    pub body: &'a [u8],
    /// Looks up an allow-listed request header.
    pub header: &'a (dyn Fn(&str) -> Option<String> + Sync),
    /// A model entry to use whatever the rules and the model name say (a budget fallback).
    pub force_entry: Option<&'a str>,
    /// Whether the request is for embeddings, which only OpenAI-protocol upstreams serve.
    pub embeddings: bool,
}

fn rule_to_entry(rule: &CompiledRule, table: &ModelTable) -> Option<Arc<ModelEntry>> {
    match &rule.action {
        Action::Entry(name) => table.entry_by_name(name).cloned(),
        Action::Targets(targets) => Some(Arc::new(ModelEntry {
            pattern: rule.name.clone(),
            name: rule.name.clone(),
            strategy: StrategyName::Fallback,
            targets: targets.clone(),
        })),
    }
}

/// Plans the route for a request.
///
/// # Errors
///
/// `404 not_found_error` when no entry answers the model name, and `400 invalid_request_error`
/// naming the unmet requirement when every target is skipped (`prompt is too long` when only the
/// context window is at fault, so the client compacts).
pub async fn plan(
    table: &ModelTable,
    rules: &RuleSet,
    catalog: &Catalog,
    state: &RouterState,
    config: &Config,
    input: &PlanInput<'_>,
) -> Result<Route, GatewayError> {
    let facts = input.facts;
    let classifier_input = super::classifier::input_from_body(facts.inbound, input.body, facts);
    let hour = u8::try_from(state.clock.unix_secs() / 3600 % 24).unwrap_or(0);
    let ctx = RuleCtx {
        facts,
        client: input.client,
        header: input.header,
        hour,
    };
    let forced = input
        .force_entry
        .and_then(|n| table.entry_by_name(n))
        .cloned();
    let selected = if forced.is_some() {
        None
    } else {
        rules.select(&ctx, &classifier_input).await
    };

    let chosen = forced.map(|e| (e, None)).or_else(|| {
        selected.and_then(|r| rule_to_entry(r, table).map(|e| (e, Some(r.name.clone()))))
    });
    let (entry, rule, via) = match chosen {
        Some((e, name)) => (e, name, None),
        None => match table.resolve_via(&facts.model, facts.inbound) {
            (Resolution::Entry(e) | Resolution::Direct(e), via) if !e.targets.is_empty() => {
                (e, None, via)
            }
            _ => {
                return Err(GatewayError::new(
                    ErrorKind::NotFound,
                    format!(
                        "the model `{}` is not mapped to an upstream model; see `airmate models`",
                        facts.model
                    ),
                ))
            }
        },
    };

    let (mut ok, mut skipped) = filter(&entry.targets, facts, catalog);
    if input.embeddings {
        ok.retain(|t| {
            let serves = matches!(t.upstream.protocol, Protocol::Chat | Protocol::Responses);
            if !serves {
                skipped.push(Skip {
                    target: t.label(),
                    reason: SkipReason::NoEmbeddings,
                });
            }
            serves
        });
    }
    if ok.is_empty() {
        return Err(unmet_error(&entry, &skipped, facts));
    }

    let candidates: Vec<Candidate<'_>> = ok
        .into_iter()
        .map(|t| {
            let st = state.target(&t.label());
            let health = state.upstream_health(&t.upstream.name);
            let healthy = !health.is_unhealthy()
                && st.breaker.snapshot(state.clock.now()).state != "open"
                && !st.is_cooling(state.clock.now());
            Candidate {
                target: t,
                state: st,
                caps: catalog.for_target(t),
                healthy,
            }
        })
        .collect();
    let rr = if entry.strategy == StrategyName::RoundRobin {
        state.next_round_robin(&entry.name)
    } else {
        0
    };
    let ordered = order(entry.strategy, candidates, rr, &*state.random);

    let affinity_key = if config.server.affinity {
        super::affinity::fingerprint(facts.inbound, input.body)
    } else {
        None
    };
    let preferred = affinity_key.as_deref().and_then(|k| state.affinity_get(k));
    let ordered = prefer(ordered, preferred.as_deref());

    let targets = ordered
        .into_iter()
        .map(|c| Planned {
            health: state.upstream_health(&c.target.upstream.name),
            target: c.target.clone(),
            state: c.state,
            caps: c.caps,
        })
        .collect();
    Ok(Route {
        entry_name: entry.name.clone(),
        rule,
        via,
        strategy: entry.strategy,
        targets,
        skipped,
        affinity_key,
    })
}

fn unmet_error(entry: &ModelEntry, skipped: &[Skip], facts: &Facts) -> GatewayError {
    let only_context = !skipped.is_empty()
        && skipped
            .iter()
            .all(|s| matches!(s.reason, SkipReason::Context { .. }));
    if only_context {
        let have = skipped
            .iter()
            .filter_map(|s| {
                if let SkipReason::Context { have, .. } = s.reason {
                    Some(have)
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(0);
        return GatewayError::new(
            ErrorKind::InvalidRequest,
            format!(
                "prompt is too long: the request needs about {} tokens of context and the largest window among the targets of `{}` is {have}",
                facts.min_context(),
                entry.name
            ),
        );
    }
    let mut seen: HashMap<String, Vec<&str>> = HashMap::new();
    for s in skipped {
        seen.entry(s.reason.explain()).or_default().push(&s.target);
    }
    let mut parts: Vec<String> = seen
        .into_iter()
        .map(|(why, ts)| format!("{} ({why})", ts.join(", ")))
        .collect();
    parts.sort();
    GatewayError::new(
        ErrorKind::InvalidRequest,
        format!(
            "no target for `{}` can serve this request: {}",
            entry.name,
            parts.join("; ")
        ),
    )
}

/// The error when every candidate was tried or skipped without an answer.
#[must_use]
pub fn exhausted(entry: &str, skipped: &[Skip]) -> GatewayError {
    let detail = if skipped.is_empty() {
        String::new()
    } else {
        format!(
            ": {}",
            skipped
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    GatewayError::new(
        ErrorKind::Overloaded,
        format!("every target for `{entry}` is unavailable{detail}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{
        FlavorName, HourRange, ModelConfig, ProtocolName, RuleConfig, RuleWhen, TargetConfig,
        Timeouts, UpstreamConfig,
    };
    use crate::router::breaker::BreakerConfig;
    use crate::router::clock::{FixedRandom, ManualClock};
    use airmate_core::ir::Protocol;
    use std::time::Duration;

    fn up(name: &str) -> UpstreamConfig {
        UpstreamConfig {
            name: name.into(),
            url: format!("http://{name}.local"),
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

    fn tgt(up: &str, model: &str) -> TargetConfig {
        TargetConfig {
            upstream: up.into(),
            model: model.into(),
            weight: None,
            effort_levels: None,
            max_output_tokens: None,
            price: None,
        }
    }

    fn entry(
        pattern: &str,
        name: Option<&str>,
        strategy: StrategyName,
        targets: Vec<TargetConfig>,
    ) -> ModelConfig {
        ModelConfig {
            pattern: pattern.into(),
            name: name.map(str::to_owned),
            strategy,
            targets,
        }
    }

    fn facts(model: &str) -> Facts {
        Facts {
            inbound: Protocol::Anthropic,
            model: model.into(),
            stream: false,
            tools: false,
            images: false,
            thinking: false,
            structured_output: false,
            effort: None,
            est_input_tokens: 100,
            max_tokens: 100,
        }
    }

    struct Harness {
        table: ModelTable,
        rules: RuleSet,
        catalog: Catalog,
        state: RouterState,
        config: Config,
        clock: Arc<ManualClock>,
    }

    fn harness(config: Config) -> Harness {
        let env: crate::config::paths::EnvFn<'_> = &|_| None;
        let (table, _) =
            ModelTable::build(&config, &crate::secret::SystemResolver::new(env)).unwrap();
        let compiled: Vec<CompiledRule> = config
            .rules
            .iter()
            .map(|r| CompiledRule {
                name: r.name.clone(),
                when: r.when.clone(),
                action: match (&r.use_entry, &r.targets) {
                    (Some(n), _) => Action::Entry(n.clone()),
                    (None, Some(ts)) => Action::Targets(
                        ts.iter()
                            .map(|t| RouteTarget {
                                upstream: table.upstream(&t.upstream).unwrap().clone(),
                                model: t.model.clone(),
                                effort_levels: None,
                                max_output_tokens: None,
                                weight: 1,
                                price: None,
                            })
                            .collect(),
                    ),
                    _ => unreachable!(),
                },
            })
            .collect();
        let clock = Arc::new(ManualClock::new());
        let cfg = BreakerConfig::from(&config.circuit);
        let state = RouterState::new(clock.clone(), Arc::new(FixedRandom::new(&[0.0])), cfg, 100);
        Harness {
            table,
            rules: RuleSet::new(compiled, HashMap::new()),
            catalog: Catalog::new(&config.catalog),
            state,
            config,
            clock,
        }
    }

    async fn plan_for(h: &Harness, f: &Facts, body: &[u8]) -> Result<Route, GatewayError> {
        let none = |_: &str| None;
        plan(
            &h.table,
            &h.rules,
            &h.catalog,
            &h.state,
            &h.config,
            &PlanInput {
                facts: f,
                client: "default",
                body,
                header: &none,
                force_entry: None,
                embeddings: false,
            },
        )
        .await
    }

    fn two_upstreams() -> Config {
        Config {
            upstreams: vec![up("fw"), up("local")],
            models: vec![entry(
                "claude-*",
                Some("claude"),
                StrategyName::Fallback,
                vec![tgt("fw", "glm"), tgt("local", "qwen3")],
            )],
            ..Config::default()
        }
    }

    fn names(r: &Route) -> Vec<String> {
        r.targets.iter().map(|p| p.target.label()).collect()
    }

    #[tokio::test]
    async fn the_model_table_orders_targets_by_the_entrys_strategy() {
        let h = harness(two_upstreams());
        let r = plan_for(&h, &facts("claude-x"), b"{}").await.unwrap();
        assert_eq!((r.entry_name.as_str(), r.rule.as_deref()), ("claude", None));
        assert_eq!(names(&r), ["fw/glm", "local/qwen3"]);
        assert!(r.skipped.is_empty());
    }

    #[tokio::test]
    async fn an_unmapped_family_name_is_not_found() {
        let h = harness(Config {
            upstreams: vec![up("fw")],
            ..Config::default()
        });
        let e = plan_for(&h, &facts("claude-x"), b"{}").await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::NotFound);
        assert!(e.message.contains("claude-x"));
    }

    #[tokio::test]
    async fn a_rule_overrides_the_table_and_may_carry_its_own_targets() {
        let mut c = two_upstreams();
        c.models.push(entry(
            "*",
            Some("cheap"),
            StrategyName::Fallback,
            vec![tgt("local", "tiny")],
        ));
        c.rules = vec![RuleConfig {
            name: "small".into(),
            when: RuleWhen {
                tools: Some(false),
                est_input_tokens_max: Some(4000),
                ..RuleWhen::default()
            },
            use_entry: Some("cheap".into()),
            targets: None,
        }];
        let h = harness(c.clone());
        let r = plan_for(&h, &facts("claude-x"), b"{}").await.unwrap();
        assert_eq!(
            (r.entry_name.as_str(), r.rule.as_deref()),
            ("cheap", Some("small"))
        );
        assert_eq!(names(&r), ["local/tiny"]);
        let mut big = facts("claude-x");
        big.est_input_tokens = 9000;
        assert_eq!(
            plan_for(&h, &big, b"{}").await.unwrap().rule,
            None,
            "the rule does not match, so the table decides"
        );

        c.rules = vec![RuleConfig {
            name: "inline".into(),
            when: RuleWhen::default(),
            use_entry: None,
            targets: Some(vec![tgt("fw", "special")]),
        }];
        let r = plan_for(&harness(c), &facts("claude-x"), b"{}")
            .await
            .unwrap();
        assert_eq!(
            (r.entry_name.as_str(), names(&r)),
            ("inline", vec!["fw/special".to_owned()])
        );
    }

    #[tokio::test]
    async fn time_of_day_rules_follow_the_clock() {
        let mut c = two_upstreams();
        c.models.push(entry(
            "*",
            Some("night"),
            StrategyName::Fallback,
            vec![tgt("local", "tiny")],
        ));
        c.rules = vec![RuleConfig {
            name: "night".into(),
            when: RuleWhen {
                hour_of_day: Some(HourRange { from: 22, to: 6 }),
                ..RuleWhen::default()
            },
            use_entry: Some("night".into()),
            targets: None,
        }];
        let h = harness(c);
        h.clock.set_unix(23 * 3600);
        assert_eq!(
            plan_for(&h, &facts("claude-x"), b"{}")
                .await
                .unwrap()
                .rule
                .as_deref(),
            Some("night")
        );
        h.clock.set_unix(12 * 3600);
        assert_eq!(
            plan_for(&h, &facts("claude-x"), b"{}").await.unwrap().rule,
            None
        );
    }

    #[tokio::test]
    async fn targets_that_lack_a_capability_are_skipped_and_recorded() {
        let mut c = two_upstreams();
        c.catalog = vec![crate::config::schema::CatalogOverride {
            pattern: "qwen3".into(),
            vision: Some(false),
            ..Default::default()
        }];
        let h = harness(c);
        let mut f = facts("claude-x");
        f.images = true;
        let r = plan_for(&h, &f, b"{}").await.unwrap();
        assert_eq!(names(&r), ["fw/glm"]);
        assert_eq!(
            r.skipped
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["local/qwen3:vision"]
        );
    }

    #[tokio::test]
    async fn when_nothing_can_serve_the_error_names_the_unmet_requirement() {
        let mut c = two_upstreams();
        c.catalog = vec![crate::config::schema::CatalogOverride {
            pattern: "*".into(),
            vision: Some(false),
            ..Default::default()
        }];
        let mut f = facts("claude-x");
        f.images = true;
        let e = plan_for(&harness(c), &f, b"{}").await.unwrap_err();
        assert_eq!(e.kind, ErrorKind::InvalidRequest);
        assert!(
            e.message.contains("cannot read images")
                && e.message.contains("fw/glm")
                && e.message.contains("local/qwen3"),
            "{}",
            e.message
        );
    }

    #[tokio::test]
    async fn when_only_the_context_window_is_short_the_client_is_told_to_compact() {
        let mut c = two_upstreams();
        c.catalog = vec![crate::config::schema::CatalogOverride {
            pattern: "*".into(),
            context_window: Some(1000),
            ..Default::default()
        }];
        let mut f = facts("claude-x");
        f.est_input_tokens = 5000;
        let e = plan_for(&harness(c), &f, b"{}").await.unwrap_err();
        assert!(e.message.starts_with("prompt is too long"), "{}", e.message);
        assert!(e.message.contains("1000"));
    }

    #[tokio::test]
    async fn an_open_breaker_or_unhealthy_upstream_is_not_preferred_but_stays_a_fallback() {
        let h = harness(two_upstreams());
        h.state.target("fw/glm").breaker.trip(Duration::ZERO, "401");
        let r = plan_for(&h, &facts("claude-x"), b"{}").await.unwrap();
        assert_eq!(
            names(&r),
            ["fw/glm", "local/qwen3"],
            "fallback keeps the configured order; the attempt loop skips open breakers"
        );
        let mut c = two_upstreams();
        c.models[0].strategy = StrategyName::Weighted;
        let h = harness(c);
        h.state
            .upstream_health("fw")
            .set(Some("connection refused".into()));
        assert_eq!(
            names(&plan_for(&h, &facts("claude-x"), b"{}").await.unwrap())[0],
            "local/qwen3",
            "weighted leads with a healthy target"
        );
    }

    #[tokio::test]
    async fn the_conversations_previous_target_is_preferred() {
        let h = harness(two_upstreams());
        let body =
            br#"{"model":"claude-x","system":"s","messages":[{"role":"user","content":"hello"}]}"#;
        let r = plan_for(&h, &facts("claude-x"), body).await.unwrap();
        let key = r.affinity_key.clone().expect("a fingerprint");
        assert_eq!(names(&r)[0], "fw/glm");
        h.state.affinity_put(&key, "local/qwen3");
        assert_eq!(
            names(&plan_for(&h, &facts("claude-x"), body).await.unwrap()),
            ["local/qwen3", "fw/glm"]
        );
        let mut off = h.config.clone();
        off.server.affinity = false;
        let none = |_: &str| None;
        let r = plan(
            &h.table,
            &h.rules,
            &h.catalog,
            &h.state,
            &off,
            &PlanInput {
                facts: &facts("claude-x"),
                client: "default",
                body,
                header: &none,
                force_entry: None,
                embeddings: false,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            (names(&r)[0].as_str(), r.affinity_key),
            ("fw/glm", None),
            "affinity can be turned off"
        );
    }

    #[tokio::test]
    async fn round_robin_advances_per_request() {
        let mut c = two_upstreams();
        c.models[0].strategy = StrategyName::RoundRobin;
        let h = harness(c);
        let lead = |r: &Route| r.targets[0].target.label();
        assert_eq!(
            lead(&plan_for(&h, &facts("claude-x"), b"{}").await.unwrap()),
            "fw/glm"
        );
        assert_eq!(
            lead(&plan_for(&h, &facts("claude-x"), b"{}").await.unwrap()),
            "local/qwen3"
        );
        assert_eq!(
            lead(&plan_for(&h, &facts("claude-x"), b"{}").await.unwrap()),
            "fw/glm"
        );
    }

    #[test]
    fn the_exhausted_error_lists_what_was_skipped() {
        let e = exhausted(
            "claude",
            &[Skip {
                target: "fw/glm".into(),
                reason: SkipReason::CircuitOpen,
            }],
        );
        assert_eq!(e.kind, ErrorKind::Overloaded);
        assert!(e.message.contains("fw/glm:circuit-open"));
        assert!(!exhausted("claude", &[]).message.contains(':'));
    }
}
