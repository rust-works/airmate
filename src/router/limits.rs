//! Rate limits: token buckets for requests and input tokens per minute (§7.8).
//!
//! Admission uses the estimator; settlement uses the true usage once the reply is in, so a request
//! that used less than estimated gives tokens back and one that used more is charged the
//! difference. Exceeding a *client* limit is a `429` with `Retry-After`; exceeding an *upstream*
//! limit is a skip, not an error, so fallback absorbs it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::clock::Clock;
use crate::config::schema::RateLimit;

/// A token bucket refilled continuously at `per_minute / 60` tokens a second.
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    capacity: f64,
    tokens: f64,
    per_sec: f64,
    last: Duration,
}

impl Bucket {
    /// A full bucket holding one minute's allowance.
    #[must_use]
    pub fn new(per_minute: f64, now: Duration) -> Self {
        Self {
            capacity: per_minute,
            tokens: per_minute,
            per_sec: per_minute / 60.0,
            last: now,
        }
    }

    fn refill(&mut self, now: Duration) {
        let dt = now.saturating_sub(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * self.per_sec).min(self.capacity);
        self.last = now.max(self.last);
    }

    /// Takes `n` tokens, or says how long until they are available. A request larger than the
    /// whole bucket is admitted when the bucket is full, and charged as a debt, so a big request
    /// is slowed down rather than refused for ever.
    ///
    /// # Errors
    ///
    /// The wait until `n` tokens (or a full bucket, for an oversized request) are available.
    pub fn try_take(&mut self, n: f64, now: Duration) -> Result<(), Duration> {
        // A bucket that never refills (a zero limit) would wait for ever: say an hour.
        const NEVER: Duration = Duration::from_secs(3600);
        if self.capacity <= 0.0 || self.per_sec <= 0.0 {
            return Err(NEVER);
        }
        self.refill(now);
        let need = n.min(self.capacity);
        if self.tokens >= need {
            self.tokens -= n;
            Ok(())
        } else {
            let secs = ((need - self.tokens) / self.per_sec).max(0.0);
            Err(Duration::try_from_secs_f64(secs).unwrap_or(NEVER))
        }
    }

    /// Gives tokens back (a request that used fewer than it was charged).
    pub fn refund(&mut self, n: f64) {
        self.tokens = (self.tokens + n).min(self.capacity);
    }

    /// Charges more (a request that used more than it was charged).
    pub fn charge(&mut self, n: f64) {
        self.tokens -= n;
    }

    /// The tokens currently available.
    #[must_use]
    pub fn available(&self) -> f64 {
        self.tokens
    }
}

/// Which limit refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// Requests per minute.
    Requests,
    /// Input tokens per minute.
    Tokens,
}

impl Which {
    /// A phrase for an error message.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Requests => "requests per minute",
            Self::Tokens => "input tokens per minute",
        }
    }
}

/// A refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied {
    /// Which limit.
    pub which: Which,
    /// How long until it would be admitted.
    pub retry_after: Duration,
}

/// The limits of one client or one upstream.
#[derive(Debug)]
pub struct Limiter {
    limit: RateLimit,
    rpm: Option<Mutex<Bucket>>,
    tpm: Option<Mutex<Bucket>>,
}

impl Limiter {
    /// A limiter for `limit`, full at `now`.
    #[must_use]
    pub fn new(limit: RateLimit, now: Duration) -> Self {
        Self {
            limit,
            rpm: limit
                .rpm
                .map(|r| Mutex::new(Bucket::new(f64::from(r), now))),
            tpm: limit.tpm.map(|t| Mutex::new(Bucket::new(t as f64, now))),
        }
    }

    /// The limits this limiter enforces.
    #[must_use]
    pub fn limit(&self) -> RateLimit {
        self.limit
    }

    /// Admits a request estimated at `est_tokens` input tokens, or refuses it.
    ///
    /// # Errors
    ///
    /// Which limit refused it, and for how long.
    ///
    /// # Panics
    ///
    /// If an internal lock is poisoned.
    pub fn admit(&self, est_tokens: u64, now: Duration) -> Result<(), Denied> {
        if let Some(r) = &self.rpm {
            r.lock()
                .expect("bucket lock")
                .try_take(1.0, now)
                .map_err(|retry_after| Denied {
                    which: Which::Requests,
                    retry_after,
                })?;
        }
        if let Some(t) = &self.tpm {
            if let Err(retry_after) = t
                .lock()
                .expect("bucket lock")
                .try_take(est_tokens as f64, now)
            {
                if let Some(r) = &self.rpm {
                    r.lock().expect("bucket lock").refund(1.0);
                }
                return Err(Denied {
                    which: Which::Tokens,
                    retry_after,
                });
            }
        }
        Ok(())
    }

    /// Settles the token charge once the real input size is known.
    ///
    /// # Panics
    ///
    /// If an internal lock is poisoned.
    pub fn settle(&self, estimated: u64, actual: u64) {
        if let Some(t) = &self.tpm {
            let mut b = t.lock().expect("bucket lock");
            if actual >= estimated {
                b.charge((actual - estimated) as f64);
            } else {
                b.refund((estimated - actual) as f64);
            }
        }
    }
}

/// Limiters for every client and upstream, shared by all requests and kept across reloads.
pub struct Limits {
    clock: Arc<dyn Clock>,
    clients: Mutex<HashMap<String, Arc<Limiter>>>,
    upstreams: Mutex<HashMap<String, Arc<Limiter>>>,
}

impl Limits {
    /// An empty registry.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            clients: Mutex::new(HashMap::new()),
            upstreams: Mutex::new(HashMap::new()),
        }
    }

    fn get(
        map: &Mutex<HashMap<String, Arc<Limiter>>>,
        key: &str,
        limit: RateLimit,
        now: Duration,
    ) -> Arc<Limiter> {
        let mut m = map.lock().expect("limits lock");
        match m.get(key) {
            Some(l) if l.limit() == limit => l.clone(),
            // New, or the configured limit changed: start a fresh bucket.
            _ => {
                let l = Arc::new(Limiter::new(limit, now));
                m.insert(key.to_owned(), l.clone());
                l
            }
        }
    }

    /// The limiter for a client with `limit`.
    ///
    /// # Panics
    ///
    /// If an internal lock is poisoned.
    #[must_use]
    pub fn client(&self, name: &str, limit: RateLimit) -> Arc<Limiter> {
        Self::get(&self.clients, name, limit, self.clock.now())
    }

    /// The limiter for an upstream with `limit`.
    ///
    /// # Panics
    ///
    /// If an internal lock is poisoned.
    #[must_use]
    pub fn upstream(&self, name: &str, limit: RateLimit) -> Arc<Limiter> {
        Self::get(&self.upstreams, name, limit, self.clock.now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::clock::ManualClock;

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_bucket_holds_a_minutes_allowance_and_refills_continuously() {
        let mut b = Bucket::new(60.0, s(0));
        for _ in 0..60 {
            assert!(b.try_take(1.0, s(0)).is_ok());
        }
        assert_eq!(
            b.try_take(1.0, s(0)).unwrap_err(),
            s(1),
            "one token a second"
        );
        assert!(b.try_take(1.0, s(1)).is_ok());
        assert_eq!(b.try_take(5.0, s(1)).unwrap_err(), s(5));
        assert!(b.try_take(5.0, s(6)).is_ok());
    }

    #[test]
    fn a_zero_limit_refuses_for_ever_without_panicking() {
        // Found in review: a zero-capacity bucket divided by a zero refill rate.
        let mut b = Bucket::new(0.0, Duration::ZERO);
        for t in 0..5 {
            assert_eq!(
                b.try_take(1.0, Duration::from_secs(t)),
                Err(Duration::from_secs(3600))
            );
        }
        let limiter = Limiter::new(
            RateLimit {
                rpm: Some(0),
                tpm: Some(0),
            },
            Duration::ZERO,
        );
        for _ in 0..3 {
            assert!(limiter.admit(0, Duration::ZERO).is_err());
        }
    }

    #[test]
    fn a_bucket_never_overfills() {
        let mut b = Bucket::new(10.0, s(0));
        b.refill(s(10_000));
        assert_eq!(b.available(), 10.0);
        b.refund(100.0);
        assert_eq!(b.available(), 10.0);
    }

    #[test]
    fn an_oversized_request_is_admitted_once_when_full_and_then_waits() {
        let mut b = Bucket::new(100.0, s(0));
        assert!(
            b.try_take(250.0, s(0)).is_ok(),
            "a full bucket admits it as a debt"
        );
        assert!(b.available() < 0.0);
        let wait = b.try_take(1.0, s(0)).unwrap_err();
        assert!(
            wait > s(90) && wait < s(91),
            "151 tokens of debt at 100 a minute: {wait:?}"
        );
        let mut empty = Bucket::new(100.0, s(0));
        empty.try_take(50.0, s(0)).unwrap();
        assert!(
            empty.try_take(250.0, s(0)).is_err(),
            "not admitted unless the bucket is full"
        );
    }

    #[test]
    fn the_limiter_enforces_requests_and_tokens_and_refunds_on_a_token_refusal() {
        let l = Limiter::new(
            RateLimit {
                rpm: Some(3),
                tpm: Some(1000),
            },
            s(0),
        );
        assert!(l.admit(400, s(0)).is_ok());
        assert!(l.admit(400, s(0)).is_ok());
        let d = l.admit(400, s(0)).unwrap_err();
        assert_eq!(d.which, Which::Tokens, "only 200 tokens are left");
        assert!(
            l.admit(100, s(0)).is_ok(),
            "the refused request did not consume its request slot"
        );
        let d = l.admit(1, s(0)).unwrap_err();
        assert_eq!(d.which, Which::Requests);
        assert_eq!(
            d.retry_after,
            s(20),
            "3 requests a minute is one every 20 s"
        );
    }

    #[test]
    fn settlement_gives_back_or_charges_the_difference() {
        let l = Limiter::new(
            RateLimit {
                rpm: None,
                tpm: Some(1000),
            },
            s(0),
        );
        l.admit(800, s(0)).unwrap();
        l.settle(800, 100);
        assert!(l.admit(850, s(0)).is_ok(), "700 tokens were handed back");
        l.settle(850, 2000);
        assert!(
            l.admit(1, s(0)).is_err(),
            "the overrun left the bucket in debt"
        );
        let unlimited = Limiter::new(RateLimit::default(), s(0));
        assert!(unlimited.admit(u64::MAX / 2, s(0)).is_ok());
        unlimited.settle(1, 2);
    }

    #[test]
    fn limits_are_exact_under_concurrent_load() {
        let limiter = Arc::new(Limiter::new(
            RateLimit {
                rpm: Some(50),
                tpm: None,
            },
            s(0),
        ));
        let granted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (l, g) = (limiter.clone(), granted.clone());
                std::thread::spawn(move || {
                    for _ in 0..40 {
                        if l.admit(1, s(0)).is_ok() {
                            g.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(
            granted.load(std::sync::atomic::Ordering::SeqCst),
            50,
            "exactly the limit, no more, no less"
        );
    }

    #[test]
    fn the_registry_reuses_a_limiter_until_its_configured_limit_changes() {
        let clock = Arc::new(ManualClock::new());
        let limits = Limits::new(clock.clone());
        let a = limits.client(
            "ci",
            RateLimit {
                rpm: Some(2),
                tpm: None,
            },
        );
        assert!(Arc::ptr_eq(
            &a,
            &limits.client(
                "ci",
                RateLimit {
                    rpm: Some(2),
                    tpm: None
                }
            )
        ));
        a.admit(1, clock.now()).unwrap();
        a.admit(1, clock.now()).unwrap();
        assert!(a.admit(1, clock.now()).is_err());
        let b = limits.client(
            "ci",
            RateLimit {
                rpm: Some(5),
                tpm: None,
            },
        );
        assert!(
            !Arc::ptr_eq(&a, &b),
            "a changed limit starts a fresh bucket"
        );
        assert!(b.admit(1, clock.now()).is_ok());
        let up = limits.upstream(
            "fw",
            RateLimit {
                rpm: Some(1),
                tpm: None,
            },
        );
        assert!(up.admit(1, clock.now()).is_ok() && up.admit(1, clock.now()).is_err());
        assert!(
            !Arc::ptr_eq(
                &up,
                &limits.client(
                    "fw",
                    RateLimit {
                        rpm: Some(1),
                        tpm: None
                    }
                )
            ),
            "clients and upstreams are separate namespaces"
        );
        assert_eq!(Which::Requests.describe(), "requests per minute");
        assert_eq!(Which::Tokens.describe(), "input tokens per minute");
    }
}
