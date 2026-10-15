//! Routing rules (§7.4).
//!
//! A rule is a predicate over the request plus an action. First match wins, in file order.
//! Predicate fields are all optional and ANDed: `model` (glob), `client`, `inbound`, `stream`,
//! `tools`, `images`, `thinking`, `effort` (min/max), `est_input_tokens` and `max_tokens`
//! (min/max), `header` (name to value glob; only allow-listed headers are visible),
//! `hour_of_day` (UTC, for time-of-day routing) and a classifier label.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use airmate_core::facts::Facts;
use airmate_core::ir::{Effort, Protocol};

use super::glob::glob_match;
use super::table::RouteTarget;
use crate::config::schema::{InboundName, RuleWhen};

/// What a rule's action selects.
#[derive(Debug, Clone)]
pub enum Action {
    /// A model entry, by name.
    Entry(String),
    /// An inline target list.
    Targets(Vec<RouteTarget>),
}

/// A rule ready to evaluate.
#[derive(Debug, Clone)]
pub struct CompiledRule {
    /// The rule's name, for logs and `x-airmate-rule`.
    pub name: String,
    /// The predicate.
    pub when: RuleWhen,
    /// The action.
    pub action: Action,
}

/// What a classifier is allowed to see.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassifierInput {
    /// The text of the last user message.
    pub last_user_message: Option<String>,
    /// The system prompt.
    pub system: Option<String>,
    /// A short digest.
    pub summary: Option<String>,
}

/// Labels a request. An extension trait of the library API; nothing in the core depends on any
/// particular classifier. A classifier that cannot answer returns `None` and the rules fall
/// through (fail open).
pub trait Classifier: Send + Sync {
    /// The label for `input`, if the classifier has one.
    fn classify<'a>(
        &'a self,
        input: &'a ClassifierInput,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
}

/// What a rule can match on.
pub struct RuleCtx<'a> {
    /// The request's routing facts.
    pub facts: &'a Facts,
    /// The authenticated client.
    pub client: &'a str,
    /// Looks up an allow-listed request header.
    pub header: &'a (dyn Fn(&str) -> Option<String> + Sync),
    /// The hour of the day, UTC.
    pub hour: u8,
}

fn inbound_matches(want: InboundName, got: Protocol) -> bool {
    matches!(
        (want, got),
        (InboundName::Anthropic, Protocol::Anthropic)
            | (InboundName::Chat, Protocol::Chat)
            | (InboundName::Responses, Protocol::Responses)
    )
}

/// Whether the non-classifier part of `w` matches.
#[must_use]
pub fn predicate_matches(w: &RuleWhen, c: &RuleCtx<'_>) -> bool {
    let f = c.facts;
    let in_range = |v: u64, min: Option<u64>, max: Option<u64>| {
        min.is_none_or(|m| v >= m) && max.is_none_or(|m| v <= m)
    };
    if w.model.as_ref().is_some_and(|g| !glob_match(g, &f.model)) {
        return false;
    }
    if w.client.as_ref().is_some_and(|n| n != c.client) {
        return false;
    }
    if w.inbound.is_some_and(|i| !inbound_matches(i, f.inbound)) {
        return false;
    }
    for (want, got) in [
        (w.stream, f.stream),
        (w.tools, f.tools),
        (w.images, f.images),
        (w.thinking, f.thinking),
    ] {
        if want.is_some_and(|x| x != got) {
            return false;
        }
    }
    if w.effort_min.is_some() || w.effort_max.is_some() {
        let Some(e) = f.effort else { return false };
        if w.effort_min.is_some_and(|m| e < Effort::from(m))
            || w.effort_max.is_some_and(|m| e > Effort::from(m))
        {
            return false;
        }
    }
    if !in_range(
        f.est_input_tokens,
        w.est_input_tokens_min,
        w.est_input_tokens_max,
    ) {
        return false;
    }
    if !in_range(
        u64::from(f.max_tokens),
        w.max_tokens_min.map(u64::from),
        w.max_tokens_max.map(u64::from),
    ) {
        return false;
    }
    for (name, pattern) in &w.header {
        if !(c.header)(name).is_some_and(|v| glob_match(pattern, &v)) {
            return false;
        }
    }
    if let Some(h) = w.hour_of_day {
        let (from, to, now) = (h.from, h.to, c.hour);
        let inside = if from <= to {
            now >= from && now < to
        } else {
            now >= from || now < to
        };
        if !inside {
            return false;
        }
    }
    true
}

/// The rules and the classifiers they consult.
#[derive(Default)]
pub struct RuleSet {
    rules: Vec<CompiledRule>,
    classifiers: HashMap<String, Arc<dyn Classifier>>,
}

impl RuleSet {
    /// A rule set.
    #[must_use]
    pub fn new(
        rules: Vec<CompiledRule>,
        classifiers: HashMap<String, Arc<dyn Classifier>>,
    ) -> Self {
        Self { rules, classifiers }
    }

    /// The rules, in order.
    #[must_use]
    pub fn rules(&self) -> &[CompiledRule] {
        &self.rules
    }

    /// The first rule that matches, consulting a classifier only for rules whose other
    /// conditions already match, and at most once per classifier. A missing or silent classifier
    /// means the rule does not match.
    pub async fn select<'s>(
        &'s self,
        ctx: &RuleCtx<'_>,
        input: &ClassifierInput,
    ) -> Option<&'s CompiledRule> {
        let mut labels: HashMap<&str, Option<String>> = HashMap::new();
        for rule in &self.rules {
            if !predicate_matches(&rule.when, ctx) {
                continue;
            }
            let Some(want) = &rule.when.classifier else {
                return Some(rule);
            };
            let label = match labels.get(want.name.as_str()) {
                Some(l) => l.clone(),
                None => {
                    let l = match self.classifiers.get(&want.name) {
                        Some(c) => c.classify(input).await,
                        None => None,
                    };
                    labels.insert(&want.name, l.clone());
                    l
                }
            };
            if label.as_deref() == Some(want.label.as_str()) {
                return Some(rule);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{ClassifierMatch, EffortName, HourRange};

    fn facts() -> Facts {
        Facts {
            inbound: Protocol::Anthropic,
            model: "claude-haiku-4".into(),
            stream: true,
            tools: false,
            images: false,
            thinking: false,
            structured_output: false,
            effort: None,
            est_input_tokens: 500,
            max_tokens: 1000,
        }
    }

    fn matches(w: &RuleWhen, f: &Facts, client: &str, hour: u8, headers: &[(&str, &str)]) -> bool {
        let owned: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let lookup = move |n: &str| {
            owned
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.clone())
        };
        predicate_matches(
            w,
            &RuleCtx {
                facts: f,
                client,
                header: &lookup,
                hour,
            },
        )
    }

    fn when(f: impl FnOnce(&mut RuleWhen)) -> RuleWhen {
        let mut w = RuleWhen::default();
        f(&mut w);
        w
    }

    #[test]
    fn an_empty_predicate_matches_everything() {
        assert!(matches(&RuleWhen::default(), &facts(), "x", 0, &[]));
    }

    #[test]
    fn model_client_and_inbound() {
        let f = facts();
        assert!(matches(
            &when(|w| w.model = Some("claude-haiku-*".into())),
            &f,
            "x",
            0,
            &[]
        ));
        assert!(!matches(
            &when(|w| w.model = Some("claude-opus-*".into())),
            &f,
            "x",
            0,
            &[]
        ));
        assert!(matches(
            &when(|w| w.client = Some("ci".into())),
            &f,
            "ci",
            0,
            &[]
        ));
        assert!(!matches(
            &when(|w| w.client = Some("ci".into())),
            &f,
            "laptop",
            0,
            &[]
        ));
        assert!(matches(
            &when(|w| w.inbound = Some(InboundName::Anthropic)),
            &f,
            "x",
            0,
            &[]
        ));
        assert!(!matches(
            &when(|w| w.inbound = Some(InboundName::Chat)),
            &f,
            "x",
            0,
            &[]
        ));
    }

    #[test]
    fn boolean_properties_must_equal_when_given() {
        let f = facts();
        assert!(matches(&when(|w| w.stream = Some(true)), &f, "x", 0, &[]));
        assert!(!matches(&when(|w| w.stream = Some(false)), &f, "x", 0, &[]));
        assert!(matches(&when(|w| w.tools = Some(false)), &f, "x", 0, &[]));
        assert!(!matches(&when(|w| w.images = Some(true)), &f, "x", 0, &[]));
        assert!(matches(
            &when(|w| w.thinking = Some(false)),
            &f,
            "x",
            0,
            &[]
        ));
        assert!(
            matches(
                &when(|w| {
                    w.tools = Some(false);
                    w.thinking = Some(false);
                    w.est_input_tokens_max = Some(4000);
                }),
                &f,
                "x",
                0,
                &[]
            ),
            "all conditions are ANDed, as in the spec's small-and-simple rule"
        );
    }

    #[test]
    fn effort_and_size_ranges_are_inclusive() {
        let mut f = facts();
        let w = when(|w| {
            w.effort_min = Some(EffortName::Medium);
            w.effort_max = Some(EffortName::High);
        });
        assert!(
            !matches(&w, &f, "x", 0, &[]),
            "no effort at all does not match an effort range"
        );
        f.effort = Some(Effort::Medium);
        assert!(matches(&w, &f, "x", 0, &[]));
        f.effort = Some(Effort::High);
        assert!(matches(&w, &f, "x", 0, &[]));
        f.effort = Some(Effort::Low);
        assert!(!matches(&w, &f, "x", 0, &[]));
        f.effort = Some(Effort::Max);
        assert!(!matches(&w, &f, "x", 0, &[]));
        let f = facts();
        assert!(matches(
            &when(|w| {
                w.est_input_tokens_min = Some(500);
                w.est_input_tokens_max = Some(500);
            }),
            &f,
            "x",
            0,
            &[]
        ));
        assert!(!matches(
            &when(|w| w.est_input_tokens_min = Some(501)),
            &f,
            "x",
            0,
            &[]
        ));
        assert!(!matches(
            &when(|w| w.max_tokens_max = Some(999)),
            &f,
            "x",
            0,
            &[]
        ));
        assert!(matches(
            &when(|w| w.max_tokens_min = Some(1000)),
            &f,
            "x",
            0,
            &[]
        ));
    }

    #[test]
    fn headers_match_by_glob_and_must_be_present() {
        let f = facts();
        let w = when(|w| {
            w.header.insert("x-team".into(), "data-*".into());
        });
        assert!(matches(&w, &f, "x", 0, &[("X-Team", "data-eng")]));
        assert!(!matches(&w, &f, "x", 0, &[("x-team", "web")]));
        assert!(
            !matches(&w, &f, "x", 0, &[]),
            "a missing header does not match"
        );
    }

    #[test]
    fn the_hour_of_day_range_wraps_past_midnight() {
        let f = facts();
        let day = when(|w| w.hour_of_day = Some(HourRange { from: 9, to: 17 }));
        for (h, want) in [(8, false), (9, true), (16, true), (17, false)] {
            assert_eq!(matches(&day, &f, "x", h, &[]), want, "{h}");
        }
        let night = when(|w| w.hour_of_day = Some(HourRange { from: 22, to: 6 }));
        for (h, want) in [
            (21, false),
            (22, true),
            (23, true),
            (0, true),
            (5, true),
            (6, false),
            (12, false),
        ] {
            assert_eq!(matches(&night, &f, "x", h, &[]), want, "{h}");
        }
        let all_day = when(|w| w.hour_of_day = Some(HourRange { from: 0, to: 24 }));
        assert!(matches(&all_day, &f, "x", 23, &[]));
    }

    struct Fixed(Option<&'static str>, std::sync::atomic::AtomicUsize);

    impl Classifier for Fixed {
        fn classify<'a>(
            &'a self,
            _: &'a ClassifierInput,
        ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move { self.0.map(str::to_owned) })
        }
    }

    fn rule(name: &str, w: RuleWhen) -> CompiledRule {
        CompiledRule {
            name: name.into(),
            when: w,
            action: Action::Entry("e".into()),
        }
    }

    fn ctx<'a>(f: &'a Facts, h: &'a (dyn Fn(&str) -> Option<String> + Sync)) -> RuleCtx<'a> {
        RuleCtx {
            facts: f,
            client: "x",
            header: h,
            hour: 0,
        }
    }

    #[tokio::test]
    async fn the_first_matching_rule_wins() {
        let set = RuleSet::new(
            vec![
                rule("opus", when(|w| w.model = Some("claude-opus-*".into()))),
                rule("haiku", when(|w| w.model = Some("claude-haiku-*".into()))),
                rule("anything", RuleWhen::default()),
            ],
            HashMap::new(),
        );
        let f = facts();
        let none = |_: &str| None;
        assert_eq!(
            set.select(&ctx(&f, &none), &ClassifierInput::default())
                .await
                .unwrap()
                .name,
            "haiku"
        );
        assert_eq!(set.rules().len(), 3);
        let empty = RuleSet::default();
        assert!(empty
            .select(&ctx(&f, &none), &ClassifierInput::default())
            .await
            .is_none());
    }

    #[tokio::test]
    async fn a_classifier_label_decides_and_is_asked_once() {
        let c = Arc::new(Fixed(Some("hard"), std::sync::atomic::AtomicUsize::new(0)));
        let mut classifiers: HashMap<String, Arc<dyn Classifier>> = HashMap::new();
        classifiers.insert("complexity".into(), c.clone());
        let cls = |label: &str| {
            when(|w| {
                w.classifier = Some(ClassifierMatch {
                    name: "complexity".into(),
                    label: label.into(),
                })
            })
        };
        let set = RuleSet::new(
            vec![
                rule("easy", cls("easy")),
                rule("medium", cls("medium")),
                rule("hard", cls("hard")),
            ],
            classifiers,
        );
        let f = facts();
        let none = |_: &str| None;
        assert_eq!(
            set.select(&ctx(&f, &none), &ClassifierInput::default())
                .await
                .unwrap()
                .name,
            "hard"
        );
        assert_eq!(
            c.1.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one call serves every rule that needs the label"
        );
    }

    #[tokio::test]
    async fn a_classifier_is_only_consulted_when_the_rest_of_the_rule_matches() {
        let c = Arc::new(Fixed(Some("hard"), std::sync::atomic::AtomicUsize::new(0)));
        let mut classifiers: HashMap<String, Arc<dyn Classifier>> = HashMap::new();
        classifiers.insert("complexity".into(), c.clone());
        let w = when(|w| {
            w.model = Some("gpt-*".into());
            w.classifier = Some(ClassifierMatch {
                name: "complexity".into(),
                label: "hard".into(),
            });
        });
        let set = RuleSet::new(vec![rule("r", w)], classifiers);
        let f = facts();
        let none = |_: &str| None;
        assert!(set
            .select(&ctx(&f, &none), &ClassifierInput::default())
            .await
            .is_none());
        assert_eq!(c.1.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_silent_or_missing_classifier_means_the_rules_fall_through() {
        let silent = Arc::new(Fixed(None, std::sync::atomic::AtomicUsize::new(0)));
        let mut classifiers: HashMap<String, Arc<dyn Classifier>> = HashMap::new();
        classifiers.insert("complexity".into(), silent);
        let cls = when(|w| {
            w.classifier = Some(ClassifierMatch {
                name: "complexity".into(),
                label: "hard".into(),
            })
        });
        let ghost = when(|w| {
            w.classifier = Some(ClassifierMatch {
                name: "ghost".into(),
                label: "x".into(),
            })
        });
        let set = RuleSet::new(
            vec![
                rule("a", cls),
                rule("b", ghost),
                rule("fallback", RuleWhen::default()),
            ],
            classifiers,
        );
        let f = facts();
        let none = |_: &str| None;
        assert_eq!(
            set.select(&ctx(&f, &none), &ClassifierInput::default())
                .await
                .unwrap()
                .name,
            "fallback"
        );
    }
}
