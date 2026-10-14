//! The circuit breaker.
//!
//! Per target: a rolling window of outcomes, a consecutive-failure counter and a three-state
//! breaker.
//!
//! - **closed**: normal.
//! - **open**: after `failure_threshold` consecutive retryable failures, or an error rate over the
//!   threshold in the window; calls are skipped for the cooldown (with jitter).
//! - **half-open**: after the cooldown one probe is allowed; success closes the breaker, failure
//!   re-opens it with a doubled cooldown (capped).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::clock::Random;
use crate::config::schema::CircuitConfig;

/// Breaker settings, resolved from the configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BreakerConfig {
    /// Consecutive retryable failures that open the breaker.
    pub failure_threshold: u32,
    /// The base cooldown.
    pub cooldown: Duration,
    /// The longest the cooldown grows to.
    pub max_cooldown: Duration,
    /// Outcomes the error rate is computed over.
    pub window: usize,
    /// The fewest outcomes before the error rate counts.
    pub min_samples: usize,
    /// The error rate that opens the breaker.
    pub error_rate: f64,
}

impl From<&CircuitConfig> for BreakerConfig {
    fn from(c: &CircuitConfig) -> Self {
        Self {
            failure_threshold: c.failure_threshold,
            cooldown: Duration::from_secs(c.cooldown_secs),
            max_cooldown: Duration::from_secs(c.max_cooldown_secs),
            window: c.window,
            min_samples: c.min_samples,
            error_rate: c.error_rate,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed,
    Open { until: Duration, cooldown: Duration },
    HalfOpen { probing: bool, cooldown: Duration },
}

/// Whether a call may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// The breaker is closed.
    Allowed,
    /// The breaker is half-open and this call is the probe.
    Probe,
    /// The breaker is open (or a probe is in flight); try again at `retry_at`.
    Denied {
        /// When the next probe is allowed, on the breaker's clock.
        retry_at: Duration,
    },
}

impl Admit {
    /// Whether the call may go ahead.
    #[must_use]
    pub fn allowed(self) -> bool {
        !matches!(self, Self::Denied { .. })
    }
}

/// A point-in-time view, for `airmate status` and metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// `closed`, `open` or `half-open`.
    pub state: &'static str,
    /// Consecutive retryable failures.
    pub consecutive_failures: u32,
    /// The last failure, if any.
    pub last_error: Option<String>,
    /// How long until the next probe, when open.
    pub next_probe_in: Option<Duration>,
}

#[derive(Debug)]
struct Inner {
    state: State,
    consecutive: u32,
    window: VecDeque<bool>,
    last_error: Option<String>,
}

/// One target's breaker.
pub struct Breaker {
    cfg: BreakerConfig,
    random: Arc<dyn Random>,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for Breaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Breaker")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl Breaker {
    /// A closed breaker.
    #[must_use]
    pub fn new(cfg: BreakerConfig, random: Arc<dyn Random>) -> Self {
        Self {
            cfg,
            random,
            inner: Mutex::new(Inner {
                state: State::Closed,
                consecutive: 0,
                window: VecDeque::new(),
                last_error: None,
            }),
        }
    }

    fn jittered(&self, cooldown: Duration) -> Duration {
        // Up to 20% longer, so a fleet of breakers does not probe in lockstep.
        cooldown.mul_f64(1.0 + 0.2 * self.random.unit())
    }

    /// Asks whether a call may proceed at `now`.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn try_acquire(&self, now: Duration) -> Admit {
        let mut i = self.inner.lock().expect("breaker lock");
        match i.state {
            State::Closed => Admit::Allowed,
            State::Open { until, cooldown } => {
                if now >= until {
                    i.state = State::HalfOpen {
                        probing: true,
                        cooldown,
                    };
                    Admit::Probe
                } else {
                    Admit::Denied { retry_at: until }
                }
            }
            State::HalfOpen {
                probing: false,
                cooldown,
            } => {
                i.state = State::HalfOpen {
                    probing: true,
                    cooldown,
                };
                Admit::Probe
            }
            State::HalfOpen { probing: true, .. } => Admit::Denied {
                retry_at: now + Duration::from_secs(1),
            },
        }
    }

    /// Records a success.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn on_success(&self) {
        let mut i = self.inner.lock().expect("breaker lock");
        i.consecutive = 0;
        Self::push(&mut i, self.cfg.window, true);
        if matches!(i.state, State::HalfOpen { .. }) {
            i.state = State::Closed;
            i.window.clear();
        }
    }

    /// Records a retryable failure.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn on_failure(&self, now: Duration, message: &str) {
        let mut i = self.inner.lock().expect("breaker lock");
        i.consecutive += 1;
        i.last_error = Some(message.to_owned());
        Self::push(&mut i, self.cfg.window, false);
        match i.state {
            State::HalfOpen { cooldown, .. } => {
                // The probe failed: back off for twice as long, up to the cap.
                let next = (cooldown * 2).min(self.cfg.max_cooldown);
                i.state = State::Open {
                    until: now + self.jittered(next),
                    cooldown: next,
                };
            }
            State::Closed => {
                let over_rate = i.window.len() >= self.cfg.min_samples.max(1) && {
                    let failures = i.window.iter().filter(|ok| !**ok).count();
                    failures as f64 / i.window.len() as f64 > self.cfg.error_rate
                };
                if i.consecutive >= self.cfg.failure_threshold || over_rate {
                    i.state = State::Open {
                        until: now + self.jittered(self.cfg.cooldown),
                        cooldown: self.cfg.cooldown,
                    };
                }
            }
            State::Open { .. } => {}
        }
    }

    /// Opens the breaker at once (a rejected API key: hammering it is harmful).
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn trip(&self, now: Duration, message: &str) {
        let mut i = self.inner.lock().expect("breaker lock");
        i.last_error = Some(message.to_owned());
        i.consecutive = self.cfg.failure_threshold;
        let cooldown = match i.state {
            State::HalfOpen { cooldown, .. } | State::Open { cooldown, .. } => {
                (cooldown * 2).min(self.cfg.max_cooldown)
            }
            State::Closed => self.cfg.cooldown,
        };
        i.state = State::Open {
            until: now + self.jittered(cooldown),
            cooldown,
        };
    }

    fn push(i: &mut Inner, window: usize, ok: bool) {
        i.window.push_back(ok);
        while i.window.len() > window.max(1) {
            i.window.pop_front();
        }
    }

    /// A view of the breaker at `now`.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    #[must_use]
    pub fn snapshot(&self, now: Duration) -> Snapshot {
        let i = self.inner.lock().expect("breaker lock");
        let (state, next_probe_in) = match i.state {
            State::Closed => ("closed", None),
            State::Open { until, .. } => ("open", Some(until.saturating_sub(now))),
            State::HalfOpen { .. } => ("half-open", Some(Duration::ZERO)),
        };
        Snapshot {
            state,
            consecutive_failures: i.consecutive,
            last_error: i.last_error.clone(),
            next_probe_in,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::clock::FixedRandom;

    fn cfg() -> BreakerConfig {
        BreakerConfig {
            failure_threshold: 3,
            cooldown: Duration::from_secs(30),
            max_cooldown: Duration::from_secs(120),
            window: 10,
            min_samples: 5,
            error_rate: 0.5,
        }
    }

    fn breaker() -> Breaker {
        // No jitter, so the arithmetic is exact.
        Breaker::new(cfg(), Arc::new(FixedRandom::new(&[0.0])))
    }

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn it_starts_closed_and_allows_calls() {
        let b = breaker();
        assert_eq!(b.try_acquire(s(0)), Admit::Allowed);
        assert_eq!(
            b.snapshot(s(0)),
            Snapshot {
                state: "closed",
                consecutive_failures: 0,
                last_error: None,
                next_probe_in: None
            }
        );
    }

    #[test]
    fn consecutive_failures_open_it_and_a_success_resets_the_count() {
        // A large min_samples keeps the error rate out of the way of this test.
        let b = Breaker::new(
            BreakerConfig {
                min_samples: 10,
                ..cfg()
            },
            Arc::new(FixedRandom::new(&[0.0])),
        );
        b.on_failure(s(0), "e1");
        b.on_failure(s(0), "e2");
        b.on_success();
        b.on_failure(s(0), "e3");
        b.on_failure(s(0), "e4");
        assert_eq!(
            b.try_acquire(s(0)),
            Admit::Allowed,
            "two in a row after a success: still closed"
        );
        b.on_failure(s(1), "e5");
        assert_eq!(b.try_acquire(s(1)), Admit::Denied { retry_at: s(31) });
        let snap = b.snapshot(s(11));
        assert_eq!(
            (
                snap.state,
                snap.consecutive_failures,
                snap.last_error.as_deref()
            ),
            ("open", 3, Some("e5"))
        );
        assert_eq!(snap.next_probe_in, Some(s(20)));
    }

    #[test]
    fn after_the_cooldown_one_probe_is_allowed_and_others_wait() {
        let b = breaker();
        for _ in 0..3 {
            b.on_failure(s(0), "e");
        }
        assert!(!b.try_acquire(s(29)).allowed());
        assert_eq!(b.try_acquire(s(30)), Admit::Probe);
        assert!(!b.try_acquire(s(30)).allowed(), "only one probe at a time");
        assert_eq!(b.snapshot(s(30)).state, "half-open");
    }

    #[test]
    fn a_successful_probe_closes_the_breaker() {
        let b = breaker();
        for _ in 0..3 {
            b.on_failure(s(0), "e");
        }
        assert_eq!(b.try_acquire(s(30)), Admit::Probe);
        b.on_success();
        assert_eq!(b.try_acquire(s(31)), Admit::Allowed);
        assert_eq!(b.snapshot(s(31)).state, "closed");
        // The cooldown starts over from the base the next time.
        for _ in 0..3 {
            b.on_failure(s(100), "e");
        }
        assert_eq!(b.try_acquire(s(100)), Admit::Denied { retry_at: s(130) });
    }

    #[test]
    fn a_failed_probe_reopens_with_a_doubled_cooldown_up_to_the_cap() {
        let b = breaker();
        for _ in 0..3 {
            b.on_failure(s(0), "e");
        }
        let mut now = 30;
        for expected_cooldown in [60, 120, 120] {
            assert_eq!(b.try_acquire(s(now)), Admit::Probe);
            b.on_failure(s(now), "still down");
            assert_eq!(
                b.try_acquire(s(now)),
                Admit::Denied {
                    retry_at: s(now + expected_cooldown)
                }
            );
            now += expected_cooldown;
        }
    }

    #[test]
    fn an_error_rate_over_the_threshold_opens_it_even_without_consecutive_failures() {
        let b = breaker();
        // Alternate failure and success: never 3 in a row, but 60% fail over the window.
        let pattern = [false, true, false, true, false, false, true, false];
        let mut opened = false;
        for (n, ok) in pattern.iter().enumerate() {
            if *ok {
                b.on_success();
            } else {
                b.on_failure(s(n as u64), "e");
            }
            opened |= b.snapshot(s(n as u64)).state == "open";
        }
        assert!(opened, "the error rate should have opened the breaker");
    }

    #[test]
    fn the_error_rate_needs_enough_samples() {
        let b = breaker();
        b.on_failure(s(0), "e");
        b.on_success();
        b.on_failure(s(0), "e");
        assert_eq!(
            b.snapshot(s(0)).state,
            "closed",
            "2 of 3 fail but there are too few samples"
        );
    }

    #[test]
    fn tripping_opens_it_at_once_and_repeats_double_the_cooldown() {
        let b = breaker();
        b.trip(s(0), "401 from the upstream");
        assert_eq!(b.try_acquire(s(0)), Admit::Denied { retry_at: s(30) });
        assert_eq!(
            b.snapshot(s(0)).last_error.as_deref(),
            Some("401 from the upstream")
        );
        assert_eq!(b.try_acquire(s(30)), Admit::Probe);
        b.trip(s(30), "401 again");
        assert_eq!(b.try_acquire(s(30)), Admit::Denied { retry_at: s(90) });
    }

    #[test]
    fn jitter_stretches_the_cooldown_by_up_to_twenty_percent() {
        let b = Breaker::new(cfg(), Arc::new(FixedRandom::new(&[0.5])));
        for _ in 0..3 {
            b.on_failure(s(0), "e");
        }
        assert_eq!(
            b.try_acquire(s(0)),
            Admit::Denied {
                retry_at: Duration::from_secs_f64(33.0)
            }
        );
    }

    #[test]
    fn failures_while_open_change_nothing_and_the_window_is_bounded() {
        let b = breaker();
        for _ in 0..3 {
            b.on_failure(s(0), "e");
        }
        b.on_failure(s(1), "more");
        assert_eq!(b.try_acquire(s(1)), Admit::Denied { retry_at: s(30) });
        let b = breaker();
        for _ in 0..100 {
            b.on_success();
        }
        assert_eq!(b.snapshot(s(0)).state, "closed");
    }

    #[test]
    fn settings_come_from_the_circuit_configuration() {
        let c = CircuitConfig::default();
        let cfg = BreakerConfig::from(&c);
        assert_eq!(
            (cfg.failure_threshold, cfg.cooldown, cfg.max_cooldown),
            (5, s(30), s(600))
        );
        assert_eq!((cfg.window, cfg.min_samples), (20, 10));
        assert!(format!("{:?}", breaker()).contains("Breaker"));
    }
}
