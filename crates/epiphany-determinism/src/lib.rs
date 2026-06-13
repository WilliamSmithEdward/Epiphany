//! Deterministic primitives for Epiphany.
//!
//! Determinism is a hard project requirement (see `docs/ROADMAP.md` section 1,
//! "Testability & determinism mandate"). Logic must never read the wall clock,
//! a random source, or unordered iteration directly. Instead it takes these
//! primitives, so tests can pin them and get byte-identical results every run.

use std::sync::atomic::{AtomicU64, Ordering};

/// A source of "now", injectable so logic never calls the wall clock directly.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_millis(&self) -> u64;
}

/// A deterministic clock for tests: starts at a fixed instant and only moves
/// when explicitly advanced.
#[derive(Debug)]
pub struct ManualClock {
    millis: AtomicU64,
}

impl ManualClock {
    pub fn new(start_millis: u64) -> Self {
        Self {
            millis: AtomicU64::new(start_millis),
        }
    }

    /// Advance the clock by `delta_millis`; returns the new value.
    pub fn advance(&self, delta_millis: u64) -> u64 {
        self.millis.fetch_add(delta_millis, Ordering::SeqCst) + delta_millis
    }

    /// Set the clock to an absolute value.
    pub fn set(&self, millis: u64) {
        self.millis.store(millis, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_millis(&self) -> u64 {
        self.millis.load(Ordering::SeqCst)
    }
}

/// A real wall clock, for production use only. Never use on deterministic paths.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A small, fast, fully deterministic PRNG (SplitMix64).
///
/// Deliberately dependency-free and reproducible: the same seed yields the same
/// sequence on every platform.
#[derive(Debug, Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Next float in `[0, 1)`, using 53 bits of mantissa.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A deterministic, monotonic id generator.
#[derive(Debug)]
pub struct IdGen {
    next: AtomicU64,
}

impl IdGen {
    pub fn starting_at(first: u64) -> Self {
        Self {
            next: AtomicU64::new(first),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next.fetch_add(1, Ordering::SeqCst)
    }
}

impl Default for IdGen {
    fn default() -> Self {
        Self::starting_at(1)
    }
}

/// A bundle of deterministic primitives for use as a test context.
#[derive(Debug)]
pub struct Deterministic {
    pub clock: ManualClock,
    pub rng: DeterministicRng,
    pub ids: IdGen,
}

impl Deterministic {
    /// A fixed, documented starting instant: 2020-01-01T00:00:00Z.
    pub const EPOCH_2020_MILLIS: u64 = 1_577_836_800_000;

    pub fn with_seed(seed: u64) -> Self {
        Self {
            clock: ManualClock::new(Self::EPOCH_2020_MILLIS),
            rng: DeterministicRng::new(seed),
            ids: IdGen::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_reproducible_for_a_seed() {
        let mut a = DeterministicRng::new(42);
        let mut b = DeterministicRng::new(42);
        let seq_a: Vec<u64> = (0..8).map(|_| a.next_u64()).collect();
        let seq_b: Vec<u64> = (0..8).map(|_| b.next_u64()).collect();
        assert_eq!(seq_a, seq_b, "same seed must produce the same sequence");
    }

    #[test]
    fn rng_differs_across_seeds() {
        let mut a = DeterministicRng::new(1);
        let mut b = DeterministicRng::new(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn rng_floats_are_in_unit_interval() {
        let mut r = DeterministicRng::new(7);
        for _ in 0..1000 {
            let x = r.next_f64();
            assert!((0.0..1.0).contains(&x));
        }
    }

    #[test]
    fn manual_clock_only_moves_when_advanced() {
        let c = ManualClock::new(1000);
        assert_eq!(c.now_millis(), 1000);
        assert_eq!(c.advance(500), 1500);
        assert_eq!(c.now_millis(), 1500);
    }

    #[test]
    fn ids_are_monotonic() {
        let g = IdGen::default();
        assert_eq!(g.next_id(), 1);
        assert_eq!(g.next_id(), 2);
        assert_eq!(g.next_id(), 3);
    }

    #[test]
    fn deterministic_context_starts_at_fixed_instant() {
        let d = Deterministic::with_seed(99);
        assert_eq!(d.clock.now_millis(), Deterministic::EPOCH_2020_MILLIS);
    }
}
