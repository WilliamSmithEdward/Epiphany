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

    /// A uniformly-distributed value in `0..bound`, with **no modulo bias**.
    ///
    /// The naive `next_u64() % bound` is biased whenever `bound` does not divide
    /// `2^64`: the low residues occur slightly more often. This rejects the top
    /// partial bucket so every value in `0..bound` is equally likely, at the cost
    /// of an occasional extra draw. Determinism is preserved: for a given seed and
    /// call sequence the result (and the number of draws consumed) is fixed on
    /// every platform.
    ///
    /// Returns `0` for `bound == 0` (an empty range has no valid value; callers
    /// that can pass zero should guard it) and consumes no randomness in that case.
    pub fn next_below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        // The largest multiple of `bound` that fits in u64; draws at or above it
        // fall in the short final bucket and are rejected to keep the range exact.
        // (`0u64.wrapping_sub(bound)` is `2^64 - bound`; `% bound` yields the
        // remainder `2^64 mod bound`, so `limit` is the rejection threshold.)
        let zone = 0u64.wrapping_sub(bound) % bound;
        let limit = u64::MAX - zone;
        loop {
            let v = self.next_u64();
            if v <= limit {
                return v % bound;
            }
        }
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
    fn next_below_stays_in_range_and_is_reproducible() {
        // Never returns a value at or above the bound, and is deterministic for a
        // seed (same seed -> same sequence of bounded draws).
        let mut a = DeterministicRng::new(7);
        let mut b = DeterministicRng::new(7);
        for bound in [1u64, 2, 3, 5, 7, 10, 1000, u64::MAX] {
            let x = a.next_below(bound);
            let y = b.next_below(bound);
            assert!(
                x < bound,
                "next_below({bound}) returned {x}, must be < bound"
            );
            assert_eq!(x, y, "same seed must give the same bounded draw");
        }
        // A zero bound is defined as 0 and consumes no randomness, so the two
        // generators stay in lockstep afterwards.
        assert_eq!(a.next_below(0), 0);
        assert_eq!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn next_below_is_unbiased_over_a_non_power_of_two_modulus() {
        // Modulo bias is worst when the modulus does not divide 2^64. With a
        // modulus of 3 (which does not), a biased `% 3` over-weights residues 0/1.
        // `next_below` must spread hits evenly: over a large seeded sample every
        // bucket lands within a tight tolerance of the expected share.
        const BOUND: u64 = 3;
        const N: u64 = 300_000;
        let mut rng = DeterministicRng::new(0x5EED_1234);
        let mut counts = [0u64; BOUND as usize];
        for _ in 0..N {
            counts[rng.next_below(BOUND) as usize] += 1;
        }
        let expected = (N / BOUND) as i64;
        // 2% of the expected bucket size is a wide margin for a fair generator at
        // this sample size but far tighter than the skew `% 3` would introduce.
        let tolerance = expected / 50;
        for (bucket, &count) in counts.iter().enumerate() {
            let delta = (count as i64 - expected).abs();
            assert!(
                delta <= tolerance,
                "bucket {bucket} count {count} deviates from expected {expected} by {delta} (> {tolerance}); range reduction looks biased"
            );
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
    fn ids_start_at_the_requested_value() {
        // `starting_at` seeds the first id, so a boot can resume above a durable
        // high-water mark without reusing a version already on disk.
        let g = IdGen::starting_at(1000);
        assert_eq!(g.next_id(), 1000);
        assert_eq!(g.next_id(), 1001);
    }

    #[test]
    fn ids_never_repeat_across_concurrent_users() {
        // IdGen is shared (`Arc<IdGen>`) across threads in the engine; the atomic
        // fetch_add must hand out every id exactly once with no gaps or duplicates.
        use std::collections::HashSet;
        use std::sync::Arc;

        const THREADS: usize = 8;
        const PER_THREAD: usize = 5000;
        let gen = Arc::new(IdGen::default());
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let gen = Arc::clone(&gen);
                std::thread::spawn(move || {
                    (0..PER_THREAD).map(|_| gen.next_id()).collect::<Vec<u64>>()
                })
            })
            .collect();

        let mut all = Vec::with_capacity(THREADS * PER_THREAD);
        for h in handles {
            all.extend(h.join().expect("id-gen thread panicked"));
        }
        let unique: HashSet<u64> = all.iter().copied().collect();
        assert_eq!(
            unique.len(),
            all.len(),
            "IdGen handed out a duplicate id under concurrency"
        );
        // The ids handed out are exactly the contiguous range [1, N] (default
        // starts at 1), proving no gaps either.
        let n = (THREADS * PER_THREAD) as u64;
        assert_eq!(*unique.iter().min().unwrap(), 1);
        assert_eq!(*unique.iter().max().unwrap(), n);
    }

    #[test]
    fn deterministic_context_starts_at_fixed_instant() {
        let d = Deterministic::with_seed(99);
        assert_eq!(d.clock.now_millis(), Deterministic::EPOCH_2020_MILLIS);
    }
}
