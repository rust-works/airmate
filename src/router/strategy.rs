//! Strategies: how the targets of a model entry are ordered (§7.6).
//!
//! - `fallback` (default): targets in order;
//! - `weighted`: weighted random selection among healthy targets, the remainder as ordered
//!   fallbacks;
//! - `round_robin`;
//! - `least_latency`: the lowest exponentially weighted time to first byte among healthy
//!   targets, with a minimum sample count and an exploration floor so a slow target can recover;
//! - `least_cost`: the cheapest by price among the targets that can serve the request.

use std::sync::Arc;

use super::catalog::Capabilities;
use super::clock::Random;
use super::state::TargetState;
use super::table::RouteTarget;
use crate::config::schema::StrategyName;

/// The fewest time-to-first-byte samples before latency counts.
pub const MIN_LATENCY_SAMPLES: u32 = 5;
/// How often least-latency tries a target other than the best, so a slow target can recover.
pub const EXPLORATION: f64 = 0.1;

/// A target that can serve the request, with what ordering needs to know about it.
#[derive(Clone)]
pub struct Candidate<'a> {
    /// The target.
    pub target: &'a RouteTarget,
    /// What has been learned about it.
    pub state: Arc<TargetState>,
    /// What it can do.
    pub caps: Capabilities,
    /// Whether its breaker is not open (a healthy target).
    pub healthy: bool,
}

impl std::fmt::Debug for Candidate<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Candidate")
            .field("target", &self.target.label())
            .field("healthy", &self.healthy)
            .finish()
    }
}

fn unit_cost(c: &Candidate<'_>) -> Option<f64> {
    c.caps.price.map(|p| p.input_per_mtok + p.output_per_mtok)
}

/// Orders `candidates` for `strategy`. `rr` is the round-robin position for the entry.
#[must_use]
pub fn order<'a>(
    strategy: StrategyName,
    mut candidates: Vec<Candidate<'a>>,
    rr: u64,
    random: &dyn Random,
) -> Vec<Candidate<'a>> {
    if candidates.len() < 2 {
        return candidates;
    }
    match strategy {
        StrategyName::Fallback => candidates,
        StrategyName::RoundRobin => {
            let n = candidates.len();
            candidates.rotate_left((rr % n as u64) as usize);
            candidates
        }
        StrategyName::Weighted => {
            let healthy: Vec<usize> = (0..candidates.len())
                .filter(|i| candidates[*i].healthy)
                .collect();
            if healthy.is_empty() {
                return candidates;
            }
            let total: u64 = healthy
                .iter()
                .map(|i| u64::from(candidates[*i].target.weight.max(1)))
                .sum();
            let mut roll = random.unit() * total as f64;
            let mut pick = healthy[0];
            for i in &healthy {
                let w = f64::from(candidates[*i].target.weight.max(1));
                if roll < w {
                    pick = *i;
                    break;
                }
                roll -= w;
            }
            let chosen = candidates.remove(pick);
            candidates.insert(0, chosen);
            candidates
        }
        StrategyName::LeastLatency => {
            // Targets without enough samples are tried first so that data accumulates; the rest
            // go fastest first. Now and then a target other than the best leads.
            let key = |c: &Candidate<'_>| c.state.ttfb_ms(MIN_LATENCY_SAMPLES);
            candidates.sort_by(|a, b| match (key(a), key(b)) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(x), Some(y)) => x.total_cmp(&y),
            });
            if random.unit() < EXPLORATION {
                let other = 1 + (random.unit() * (candidates.len() - 1) as f64) as usize;
                let last = candidates.len() - 1;
                candidates.swap(0, other.min(last));
            }
            candidates
        }
        StrategyName::LeastCost => {
            candidates.sort_by(|a, b| match (unit_cost(a), unit_cost(b)) {
                (Some(x), Some(y)) => x.total_cmp(&y),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            });
            candidates
        }
    }
}

/// Moves the target that last served a conversation to the front, unless it is unhealthy or no
/// longer a candidate.
#[must_use]
pub fn prefer<'a>(
    mut candidates: Vec<Candidate<'a>>,
    preferred_label: Option<&str>,
) -> Vec<Candidate<'a>> {
    if let Some(label) = preferred_label {
        if let Some(pos) = candidates
            .iter()
            .position(|c| c.target.label() == label && c.healthy)
        {
            let c = candidates.remove(pos);
            candidates.insert(0, c);
        }
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{FlavorName, Price, ProtocolName, Timeouts, UpstreamConfig};
    use crate::router::breaker::BreakerConfig;
    use crate::router::clock::{FixedRandom, ManualClock};
    use crate::router::state::RouterState;
    use crate::router::table::UpstreamRt;
    use std::time::Duration;

    fn rt(name: &str) -> Arc<UpstreamRt> {
        let env: crate::config::paths::EnvFn<'_> = &|_| None;
        let cfg = crate::config::Config {
            upstreams: vec![UpstreamConfig {
                name: name.into(),
                url: "http://x".into(),
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
            }],
            ..crate::config::Config::default()
        };
        crate::router::ModelTable::build(&cfg, &crate::secret::SystemResolver::new(env))
            .unwrap()
            .0
            .upstream(name)
            .unwrap()
            .clone()
    }

    fn target(name: &str, weight: u32) -> RouteTarget {
        RouteTarget {
            upstream: rt(name),
            model: "m".into(),
            effort_levels: None,
            max_output_tokens: None,
            weight,
            price: None,
        }
    }

    fn state() -> RouterState {
        RouterState::new(
            Arc::new(ManualClock::new()),
            Arc::new(FixedRandom::new(&[0.5])),
            BreakerConfig {
                failure_threshold: 2,
                cooldown: Duration::from_secs(10),
                max_cooldown: Duration::from_secs(60),
                window: 10,
                min_samples: 5,
                error_rate: 0.5,
            },
            100,
        )
    }

    fn cands<'a>(ts: &'a [RouteTarget], st: &RouterState) -> Vec<Candidate<'a>> {
        ts.iter()
            .map(|t| Candidate {
                target: t,
                state: st.target(&t.label()),
                caps: Capabilities::unknown(),
                healthy: true,
            })
            .collect()
    }

    fn labels(c: &[Candidate<'_>]) -> Vec<String> {
        c.iter().map(|c| c.target.label()).collect()
    }

    #[test]
    fn fallback_keeps_the_configured_order() {
        let ts = [target("a", 1), target("b", 1), target("c", 1)];
        let st = state();
        assert_eq!(
            labels(&order(
                StrategyName::Fallback,
                cands(&ts, &st),
                7,
                &FixedRandom::new(&[0.9])
            )),
            ["a/m", "b/m", "c/m"]
        );
        assert_eq!(
            order(
                StrategyName::Weighted,
                cands(&ts[..1], &st),
                0,
                &FixedRandom::new(&[0.0])
            )
            .len(),
            1
        );
    }

    #[test]
    fn round_robin_rotates_with_the_counter() {
        let ts = [target("a", 1), target("b", 1), target("c", 1)];
        let st = state();
        let seen: Vec<String> = (0..4)
            .map(|rr| {
                labels(&order(
                    StrategyName::RoundRobin,
                    cands(&ts, &st),
                    rr,
                    &FixedRandom::new(&[0.0]),
                ))[0]
                    .clone()
            })
            .collect();
        assert_eq!(seen, ["a/m", "b/m", "c/m", "a/m"]);
        assert_eq!(
            labels(&order(
                StrategyName::RoundRobin,
                cands(&ts, &st),
                1,
                &FixedRandom::new(&[0.0])
            )),
            ["b/m", "c/m", "a/m"],
            "the rest stay as ordered fallbacks"
        );
    }

    #[test]
    fn weighted_picks_by_weight_and_keeps_the_rest_in_order() {
        let ts = [target("a", 1), target("b", 3)];
        let st = state();
        let lead = |roll: f64| {
            labels(&order(
                StrategyName::Weighted,
                cands(&ts, &st),
                0,
                &FixedRandom::new(&[roll]),
            ))[0]
                .clone()
        };
        assert_eq!(lead(0.0), "a/m");
        assert_eq!(lead(0.24), "a/m");
        assert_eq!(lead(0.26), "b/m");
        assert_eq!(lead(0.99), "b/m");
        assert_eq!(
            labels(&order(
                StrategyName::Weighted,
                cands(&ts, &st),
                0,
                &FixedRandom::new(&[0.9])
            )),
            ["b/m", "a/m"]
        );
    }

    #[test]
    fn weighted_only_leads_with_a_healthy_target() {
        let ts = [target("a", 1), target("b", 1)];
        let st = state();
        let mut c = cands(&ts, &st);
        c[1].healthy = false;
        assert_eq!(
            labels(&order(
                StrategyName::Weighted,
                c,
                0,
                &FixedRandom::new(&[0.99])
            ))[0],
            "a/m"
        );
        let mut all_down = cands(&ts, &st);
        all_down.iter_mut().for_each(|c| c.healthy = false);
        assert_eq!(
            labels(&order(
                StrategyName::Weighted,
                all_down,
                0,
                &FixedRandom::new(&[0.99])
            )),
            ["a/m", "b/m"]
        );
    }

    #[test]
    fn least_latency_tries_unmeasured_targets_first_then_the_fastest() {
        let ts = [target("a", 1), target("b", 1), target("c", 1)];
        let st = state();
        for (label, ms) in [("a/m", 300), ("b/m", 100)] {
            for _ in 0..MIN_LATENCY_SAMPLES {
                st.target(label).record_ttfb(Duration::from_millis(ms));
            }
        }
        let no_explore = FixedRandom::new(&[0.99]);
        assert_eq!(
            labels(&order(
                StrategyName::LeastLatency,
                cands(&ts, &st),
                0,
                &no_explore
            )),
            ["c/m", "b/m", "a/m"],
            "c has no data yet"
        );
        for _ in 0..MIN_LATENCY_SAMPLES {
            st.target("c/m").record_ttfb(Duration::from_millis(900));
        }
        assert_eq!(
            labels(&order(
                StrategyName::LeastLatency,
                cands(&ts, &st),
                0,
                &no_explore
            )),
            ["b/m", "a/m", "c/m"]
        );
    }

    #[test]
    fn least_latency_sometimes_explores_so_a_slow_target_can_recover() {
        let ts = [target("a", 1), target("b", 1)];
        let st = state();
        for _ in 0..MIN_LATENCY_SAMPLES {
            st.target("a/m").record_ttfb(Duration::from_millis(100));
            st.target("b/m").record_ttfb(Duration::from_millis(900));
        }
        // First roll under the exploration floor, second picks which other target leads.
        let explore = FixedRandom::new(&[0.05, 0.0]);
        assert_eq!(
            labels(&order(
                StrategyName::LeastLatency,
                cands(&ts, &st),
                0,
                &explore
            ))[0],
            "b/m"
        );
    }

    #[test]
    fn least_cost_orders_by_price_with_unpriced_targets_last() {
        let mut ts = [target("a", 1), target("b", 1), target("c", 1)];
        ts[0].price = Some(Price {
            input_per_mtok: 5.0,
            output_per_mtok: 15.0,
            cache_read_per_mtok: None,
        });
        ts[2].price = Some(Price {
            input_per_mtok: 0.5,
            output_per_mtok: 1.5,
            cache_read_per_mtok: None,
        });
        let st = state();
        let mut c = cands(&ts, &st);
        for (i, cand) in c.iter_mut().enumerate() {
            cand.caps.price = ts[i].price;
        }
        assert_eq!(
            labels(&order(
                StrategyName::LeastCost,
                c,
                0,
                &FixedRandom::new(&[0.0])
            )),
            ["c/m", "a/m", "b/m"]
        );
    }

    #[test]
    fn affinity_moves_a_healthy_preferred_target_to_the_front_only() {
        let ts = [target("a", 1), target("b", 1), target("c", 1)];
        let st = state();
        assert_eq!(
            labels(&prefer(cands(&ts, &st), Some("c/m"))),
            ["c/m", "a/m", "b/m"]
        );
        assert_eq!(
            labels(&prefer(cands(&ts, &st), Some("gone/m"))),
            ["a/m", "b/m", "c/m"]
        );
        assert_eq!(
            labels(&prefer(cands(&ts, &st), None)),
            ["a/m", "b/m", "c/m"]
        );
        let mut c = cands(&ts, &st);
        c[2].healthy = false;
        assert_eq!(
            labels(&prefer(c, Some("c/m"))),
            ["a/m", "b/m", "c/m"],
            "an unhealthy preferred target does not lead"
        );
        assert!(format!("{:?}", cands(&ts, &st)[0]).contains("a/m"));
    }
}
