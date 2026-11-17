//! Global version clock for the TL2 algorithm.
//!
//! TL2 needs one shared, monotonically increasing counter that every thread
//! can read and increment atomically. Transactions snapshot this clock when
//! they start (their "read version") and the clock is bumped once, as part
//! of the commit protocol, whenever a transaction successfully writes.
//!
//! The counter itself lives in a small [`Clock`] struct rather than directly
//! in a `static`. The library uses exactly one instance (`GLOBAL_CLOCK`), but
//! keeping the logic in a type means unit tests can create their own private
//! clock and assert exact values without being disturbed by other tests that
//! commit transactions (and therefore bump the global clock) in parallel.
//!
//! Memory ordering: [`Clock::bump`] is an `AcqRel` read-modify-write and
//! [`Clock::read`] an `Acquire` load. A committer bumps the clock only after
//! it has locked its write-set, so any transaction whose read version
//! includes that bump has *acquired* the committer's lock acquisition and is
//! guaranteed to see the lock flags set (or the values already written).
//! Because every bump is a read-modify-write, a later bump by another thread
//! continues the release sequence, so reading any later clock value gives the
//! same guarantee. `ARCHITECTURE.md` has the full argument; `tests/loom.rs`
//! checks it under every interleaving loom explores.

use crate::sync::{AtomicU64, Ordering};

/// A monotonically increasing version counter.
///
/// Starts at 0. [`Clock::bump`] is the only way to advance it, so the value
/// never goes backwards.
#[derive(Debug)]
pub struct Clock {
    counter: AtomicU64,
}

impl Clock {
    /// Create a clock starting at version 0.
    ///
    /// This is a `const fn` so a `Clock` can be used as a `static`.
    #[cfg(not(loom))]
    pub const fn new() -> Self {
        Clock {
            counter: AtomicU64::new(0),
        }
    }

    /// Create a clock starting at version 0. (`loom`'s atomics cannot be
    /// built in a `const` context, so the loom build gets a plain `fn`.)
    #[cfg(loom)]
    pub fn new() -> Self {
        Clock {
            counter: AtomicU64::new(0),
        }
    }

    /// Read the current version without modifying it.
    pub fn read(&self) -> u64 {
        self.counter.load(Ordering::Acquire)
    }

    /// Atomically add one and return the NEW value.
    ///
    /// `fetch_add` returns the value from *before* the addition, so we add
    /// one to report the version this call created. Because the increment is
    /// a single atomic read-modify-write, no two callers ever get the same
    /// number back.
    pub fn bump(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::AcqRel) + 1
    }
}

impl Default for Clock {
    fn default() -> Self {
        Clock::new()
    }
}

/// The global version clock, shared by all transactions.
#[cfg(not(loom))]
static GLOBAL_CLOCK: Clock = Clock::new();

// Under loom, every model execution must start from a fresh clock, which
// `loom::lazy_static!` provides.
#[cfg(loom)]
loom::lazy_static! {
    static ref GLOBAL_CLOCK: Clock = Clock::new();
}

/// Read the current value of the global clock without modifying it.
///
/// Used by a transaction when it starts, to record its "read version" —
/// the snapshot of the world it will validate against at commit time.
pub fn read_clock() -> u64 {
    GLOBAL_CLOCK.read()
}

/// Atomically increment the global clock and return the NEW value.
///
/// Called once per successful *writing* commit, after the write-set has been
/// locked and before the read-set is validated (see `transaction.rs`). The
/// returned value becomes the "write version" stamped onto every TVar the
/// transaction writes.
pub fn bump_clock() -> u64 {
    GLOBAL_CLOCK.bump()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn clock_starts_at_zero_and_increments() {
        // A private Clock instance, so other tests that commit transactions
        // (and bump the global clock) cannot change the numbers under us.
        let clock = Clock::new();
        assert_eq!(clock.read(), 0);
        assert_eq!(clock.bump(), 1);
        assert_eq!(clock.read(), 1);
        assert_eq!(clock.bump(), 2);
        assert_eq!(clock.read(), 2);
    }

    #[test]
    fn global_clock_never_goes_backwards() {
        // We can't assert exact values on the shared global clock, but we
        // can assert the property the algorithm needs: it only moves forward.
        let before = read_clock();
        let bumped = bump_clock();
        let after = read_clock();
        assert!(bumped > before);
        assert!(after >= bumped);
    }

    #[test]
    fn concurrent_bumps_are_unique_and_none_are_lost() {
        const THREADS: usize = 8;
        const BUMPS_PER_THREAD: usize = 5_000;

        let clock = Arc::new(Clock::new());
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let clock = Arc::clone(&clock);
                thread::spawn(move || {
                    (0..BUMPS_PER_THREAD)
                        .map(|_| clock.bump())
                        .collect::<Vec<u64>>()
                })
            })
            .collect();

        let mut seen = HashSet::new();
        for handle in handles {
            for version in handle.join().unwrap() {
                // Every bump must hand out a version nobody else received.
                assert!(seen.insert(version), "version {version} handed out twice");
            }
        }

        let total = (THREADS * BUMPS_PER_THREAD) as u64;
        assert_eq!(clock.read(), total);
        assert_eq!(seen.len() as u64, total);
    }
}
