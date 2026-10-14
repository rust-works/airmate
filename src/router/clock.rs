//! Time, injectable.
//!
//! Breakers, rate limits and rules read the clock through this trait, so tests can drive them
//! with a [`ManualClock`] instead of sleeping.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A source of time. An extension trait of the library API.
pub trait Clock: Send + Sync {
    /// Monotonic time since an arbitrary origin.
    fn now(&self) -> Duration;
    /// Wall-clock seconds since the Unix epoch (for hour-of-day rules and budget periods).
    fn unix_secs(&self) -> u64;
}

/// The real clock.
#[derive(Debug)]
pub struct SystemClock {
    origin: Instant,
}

impl SystemClock {
    /// A clock whose monotonic origin is now.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }

    fn unix_secs(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }
}

/// A clock that moves only when told to.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: Mutex<Duration>,
    unix: AtomicU64,
}

impl ManualClock {
    /// A clock at monotonic zero and Unix time zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves both clocks forward.
    ///
    /// # Panics
    ///
    /// If the internal lock is poisoned.
    pub fn advance(&self, d: Duration) {
        *self.now.lock().expect("clock lock") += d;
        self.unix.fetch_add(d.as_secs(), Ordering::SeqCst);
    }

    /// Sets the wall-clock time.
    pub fn set_unix(&self, secs: u64) {
        self.unix.store(secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        *self.now.lock().expect("clock lock")
    }

    fn unix_secs(&self) -> u64 {
        self.unix.load(Ordering::SeqCst)
    }
}

/// Where randomness comes from. An extension trait, so jitter and weighted choice are testable.
pub trait Random: Send + Sync {
    /// A number in `[0, 1)`.
    fn unit(&self) -> f64;
}

/// The OS random source.
#[derive(Debug, Default)]
pub struct OsRandom;

impl Random for OsRandom {
    fn unit(&self) -> f64 {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A fixed value, or a repeating sequence, for tests.
#[derive(Debug)]
pub struct FixedRandom {
    values: Vec<f64>,
    next: AtomicU64,
}

impl FixedRandom {
    /// Cycles through `values`.
    #[must_use]
    pub fn new(values: &[f64]) -> Self {
        Self {
            values: values.to_vec(),
            next: AtomicU64::new(0),
        }
    }
}

impl Random for FixedRandom {
    fn unit(&self) -> f64 {
        let i = self.next.fetch_add(1, Ordering::SeqCst) as usize;
        self.values[i % self.values.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manual_clock_moves_only_when_advanced() {
        let c = ManualClock::new();
        assert_eq!((c.now(), c.unix_secs()), (Duration::ZERO, 0));
        c.advance(Duration::from_secs(90));
        assert_eq!((c.now(), c.unix_secs()), (Duration::from_secs(90), 90));
        c.set_unix(1_700_000_000);
        assert_eq!(c.unix_secs(), 1_700_000_000);
        assert_eq!(c.now(), Duration::from_secs(90));
    }

    #[test]
    fn the_system_clock_is_monotonic_and_plausible() {
        let c = SystemClock::new();
        let a = c.now();
        std::thread::sleep(Duration::from_millis(5));
        assert!(c.now() > a);
        assert!(c.unix_secs() > 1_700_000_000);
        let _ = SystemClock::default();
    }

    #[test]
    fn os_randomness_is_in_range_and_varies() {
        let r = OsRandom;
        let v: Vec<f64> = (0..50).map(|_| r.unit()).collect();
        assert!(v.iter().all(|x| (0.0..1.0).contains(x)));
        assert!(v.windows(2).any(|w| (w[0] - w[1]).abs() > 1e-9));
    }

    #[test]
    fn fixed_randomness_cycles() {
        let r = FixedRandom::new(&[0.1, 0.9]);
        assert_eq!((r.unit(), r.unit(), r.unit()), (0.1, 0.9, 0.1));
    }
}
