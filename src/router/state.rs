//! Learned state about targets: breakers, latency, in-flight counts, cooling-down marks.
//!
//! It lives outside the tables that reload swaps, so a reload does not forget a breaker that is
//! open or the latency a target has shown.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::affinity::Lru;
use super::breaker::{Breaker, BreakerConfig};
use super::clock::{Clock, Random};

/// An exponentially weighted moving average with a sample count.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Ewma {
    value: f64,
    samples: u32,
}

impl Ewma {
    const ALPHA: f64 = 0.3;

    /// Adds a sample.
    pub fn add(&mut self, x: f64) {
        self.value = if self.samples == 0 {
            x
        } else {
            Self::ALPHA * x + (1.0 - Self::ALPHA) * self.value
        };
        self.samples += 1;
    }

    /// The average, once there are at least `min` samples.
    #[must_use]
    pub fn get(&self, min: u32) -> Option<f64> {
        (self.samples >= min && self.samples > 0).then_some(self.value)
    }

    /// How many samples there are.
    #[must_use]
    pub fn samples(&self) -> u32 {
        self.samples
    }
}

/// Everything learned about one target.
pub struct TargetState {
    /// The circuit breaker.
    pub breaker: Breaker,
    ttfb_ms: Mutex<Ewma>,
    /// Calls in flight.
    pub inflight: AtomicUsize,
    cooling_until: Mutex<Option<Duration>>,
    unhealthy: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for TargetState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetState").finish_non_exhaustive()
    }
}

impl TargetState {
    fn new(cfg: BreakerConfig, random: Arc<dyn Random>) -> Self {
        Self {
            breaker: Breaker::new(cfg, random),
            ttfb_ms: Mutex::new(Ewma::default()),
            inflight: AtomicUsize::new(0),
            cooling_until: Mutex::new(None),
            unhealthy: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Records a time to first byte.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn record_ttfb(&self, d: Duration) {
        self.ttfb_ms
            .lock()
            .expect("ewma lock")
            .add(d.as_secs_f64() * 1000.0);
    }

    /// The smoothed time to first byte in milliseconds, once `min` samples exist.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn ttfb_ms(&self, min: u32) -> Option<f64> {
        self.ttfb_ms.lock().expect("ewma lock").get(min)
    }

    /// How many time-to-first-byte samples exist.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn ttfb_samples(&self) -> u32 {
        self.ttfb_ms.lock().expect("ewma lock").samples()
    }

    /// Marks the target as cooling down until `until`: the upstream asked for a pause longer than
    /// the router is willing to wait.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn cool_until(&self, until: Duration) {
        *self.cooling_until.lock().expect("cooling lock") = Some(until);
    }

    /// Whether the target is cooling down at `now`.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn is_cooling(&self, now: Duration) -> bool {
        self.cooling_until
            .lock()
            .expect("cooling lock")
            .is_some_and(|u| now < u)
    }

    /// Sets the result of the active health check.
    pub fn set_unhealthy(&self, unhealthy: bool) {
        self.unhealthy.store(unhealthy, Ordering::SeqCst);
    }

    /// Whether the active health check is failing.
    #[must_use]
    pub fn is_unhealthy(&self) -> bool {
        self.unhealthy.load(Ordering::SeqCst)
    }
}

/// The result of an upstream's active health check.
#[derive(Debug, Default)]
pub struct UpstreamHealth {
    unhealthy: std::sync::atomic::AtomicBool,
    last_error: Mutex<Option<String>>,
}

impl UpstreamHealth {
    /// Whether the last probe failed.
    #[must_use]
    pub fn is_unhealthy(&self) -> bool {
        self.unhealthy.load(Ordering::SeqCst)
    }

    /// Records a probe result.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn set(&self, error: Option<String>) {
        self.unhealthy.store(error.is_some(), Ordering::SeqCst);
        *self.last_error.lock().expect("health lock") = error;
    }

    /// The last probe error, if the last probe failed.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().expect("health lock").clone()
    }
}

/// Router state shared by every request.
pub struct RouterState {
    /// Time.
    pub clock: Arc<dyn Clock>,
    /// Randomness.
    pub random: Arc<dyn Random>,
    breaker_cfg: Mutex<BreakerConfig>,
    targets: Mutex<HashMap<String, Arc<TargetState>>>,
    rr: Mutex<HashMap<String, u64>>,
    affinity: Mutex<Lru>,
    generation: AtomicU64,
    health: Mutex<HashMap<String, Arc<UpstreamHealth>>>,
    /// Upstream credentials the router has seen rejected (401 or 403).
    pub auth_alerts: AtomicU64,
}

impl RouterState {
    /// Fresh state.
    #[must_use]
    pub fn new(
        clock: Arc<dyn Clock>,
        random: Arc<dyn Random>,
        breaker_cfg: BreakerConfig,
        affinity_cap: usize,
    ) -> Self {
        Self {
            clock,
            random,
            breaker_cfg: Mutex::new(breaker_cfg),
            targets: Mutex::new(HashMap::new()),
            rr: Mutex::new(HashMap::new()),
            affinity: Mutex::new(Lru::new(affinity_cap)),
            generation: AtomicU64::new(0),
            health: Mutex::new(HashMap::new()),
            auth_alerts: AtomicU64::new(0),
        }
    }

    /// The state for a target, created on first use.
    ///
    /// # Panics
    ///
    /// If an internal lock is poisoned.
    #[must_use]
    pub fn target(&self, label: &str) -> Arc<TargetState> {
        let cfg = *self.breaker_cfg.lock().expect("cfg lock");
        self.targets
            .lock()
            .expect("targets lock")
            .entry(label.to_owned())
            .or_insert_with(|| Arc::new(TargetState::new(cfg, self.random.clone())))
            .clone()
    }

    /// The health record for an upstream, created on first use.
    ///
    /// # Panics
    ///
    /// If an internal lock is poisoned.
    #[must_use]
    pub fn upstream_health(&self, upstream: &str) -> Arc<UpstreamHealth> {
        self.health
            .lock()
            .expect("health lock")
            .entry(upstream.to_owned())
            .or_default()
            .clone()
    }

    /// Changes the breaker settings used for targets created from now on (existing breakers keep
    /// what they have until the process restarts).
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn set_breaker_config(&self, cfg: BreakerConfig) {
        *self.breaker_cfg.lock().expect("cfg lock") = cfg;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The next round-robin position for an entry.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn next_round_robin(&self, entry: &str) -> u64 {
        let mut rr = self.rr.lock().expect("rr lock");
        let n = rr.entry(entry.to_owned()).or_insert(0);
        let v = *n;
        *n += 1;
        v
    }

    /// The target that last served a conversation.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn affinity_get(&self, key: &str) -> Option<String> {
        self.affinity.lock().expect("affinity lock").get(key)
    }

    /// Remembers which target served a conversation.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn affinity_put(&self, key: &str, target: &str) {
        self.affinity
            .lock()
            .expect("affinity lock")
            .put(key, target);
    }

    /// Every known target label and its state.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn all_targets(&self) -> Vec<(String, Arc<TargetState>)> {
        let mut v: Vec<_> = self
            .targets
            .lock()
            .expect("targets lock")
            .iter()
            .map(|(k, s)| (k.clone(), s.clone()))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::clock::{FixedRandom, ManualClock};

    fn cfg() -> BreakerConfig {
        BreakerConfig {
            failure_threshold: 2,
            cooldown: Duration::from_secs(10),
            max_cooldown: Duration::from_secs(60),
            window: 10,
            min_samples: 5,
            error_rate: 0.5,
        }
    }

    fn state() -> RouterState {
        RouterState::new(
            Arc::new(ManualClock::new()),
            Arc::new(FixedRandom::new(&[0.0])),
            cfg(),
            2,
        )
    }

    #[test]
    fn the_ewma_needs_a_minimum_of_samples_and_smooths() {
        let mut e = Ewma::default();
        assert_eq!(e.get(1), None);
        e.add(100.0);
        assert_eq!(e.get(1), Some(100.0));
        assert_eq!(e.get(2), None);
        e.add(200.0);
        assert!(
            (e.get(2).unwrap() - 130.0).abs() < 1e-9,
            "0.3*200 + 0.7*100"
        );
        assert_eq!(e.samples(), 2);
    }

    #[test]
    fn target_state_is_created_once_and_shared() {
        let s = state();
        let a = s.target("fw/glm");
        let b = s.target("fw/glm");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &s.target("fw/other")));
        assert_eq!(
            s.all_targets()
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["fw/glm", "fw/other"]
        );
    }

    #[test]
    fn latency_cooling_and_health_marks() {
        let s = state();
        let t = s.target("a/b");
        assert_eq!((t.ttfb_ms(1), t.ttfb_samples()), (None, 0));
        t.record_ttfb(Duration::from_millis(250));
        assert_eq!(t.ttfb_ms(1), Some(250.0));
        assert!(!t.is_cooling(Duration::from_secs(5)));
        t.cool_until(Duration::from_secs(30));
        assert!(t.is_cooling(Duration::from_secs(5)) && !t.is_cooling(Duration::from_secs(30)));
        assert!(!t.is_unhealthy());
        t.set_unhealthy(true);
        assert!(t.is_unhealthy());
        assert_eq!(t.inflight.load(Ordering::SeqCst), 0);
        assert!(format!("{t:?}").contains("TargetState"));
    }

    #[test]
    fn round_robin_counts_per_entry() {
        let s = state();
        assert_eq!(
            (
                s.next_round_robin("a"),
                s.next_round_robin("a"),
                s.next_round_robin("b")
            ),
            (0, 1, 0)
        );
    }

    #[test]
    fn affinity_is_bounded_and_remembers_the_last_target() {
        let s = state();
        s.affinity_put("c1", "fw/a");
        s.affinity_put("c2", "fw/b");
        s.affinity_put("c3", "fw/c");
        assert_eq!(
            s.affinity_get("c1"),
            None,
            "the oldest was evicted (capacity 2)"
        );
        assert_eq!(s.affinity_get("c3").as_deref(), Some("fw/c"));
    }

    #[test]
    fn breaker_settings_can_change_for_new_targets() {
        let s = state();
        let old = s.target("x/1");
        s.set_breaker_config(BreakerConfig {
            failure_threshold: 9,
            ..cfg()
        });
        let new = s.target("x/2");
        for _ in 0..2 {
            old.breaker.on_failure(Duration::ZERO, "e");
            new.breaker.on_failure(Duration::ZERO, "e");
        }
        assert_eq!(old.breaker.snapshot(Duration::ZERO).state, "open");
        assert_eq!(new.breaker.snapshot(Duration::ZERO).state, "closed");
    }
}
