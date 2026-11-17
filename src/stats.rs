//! Per-thread commit and abort counters.
//!
//! Every attempt a transaction makes ends in exactly one of four outcomes,
//! and each outcome bumps one counter for the current thread:
//!
//! | Outcome | Counter |
//! |---|---|
//! | commit succeeded | `commits` |
//! | a read saw a newer or locked `TVar` | `read_conflicts` |
//! | commit-time validation of the read-set failed | `validation_failures` |
//! | the closure returned `TxError::Failed` | `user_aborts` |
//!
//! The counters are **thread-local** on purpose. A single global counter
//! would be one more cache line that every committing thread writes, which
//! is exactly the kind of contention STM tries to avoid. Code that wants a
//! global picture (the live dashboard in `examples/bank_demo.rs`) reads each
//! worker's [`thread_stats`] and publishes it however it likes.
//!
//! With the `trace` cargo feature enabled, every attempt is also logged to
//! stderr with its read version, read-set and write-set sizes, and outcome.

use std::cell::Cell;
use std::ops::{Add, Sub};

/// Counts of transaction outcomes on one thread. See the module docs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TxStats {
    /// Attempts that committed.
    pub commits: u64,
    /// Attempts aborted because a read found a `TVar` that was newer than
    /// the snapshot, or locked by a committer.
    pub read_conflicts: u64,
    /// Attempts aborted because commit-time validation of the read-set failed.
    pub validation_failures: u64,
    /// Attempts the closure itself ended with `TxError::Failed`.
    pub user_aborts: u64,
}

impl TxStats {
    /// All counters zero.
    pub const ZERO: TxStats = TxStats {
        commits: 0,
        read_conflicts: 0,
        validation_failures: 0,
        user_aborts: 0,
    };

    /// Attempts that were thrown away because of a conflict and retried.
    pub fn conflicts(&self) -> u64 {
        self.read_conflicts + self.validation_failures
    }

    /// Every attempt, whatever its outcome.
    pub fn attempts(&self) -> u64 {
        self.commits + self.conflicts() + self.user_aborts
    }

    /// Fraction of attempts lost to conflicts, from 0.0 to 1.0.
    pub fn conflict_rate(&self) -> f64 {
        match self.attempts() {
            0 => 0.0,
            n => self.conflicts() as f64 / n as f64,
        }
    }
}

impl Add for TxStats {
    type Output = TxStats;
    fn add(self, other: TxStats) -> TxStats {
        TxStats {
            commits: self.commits + other.commits,
            read_conflicts: self.read_conflicts + other.read_conflicts,
            validation_failures: self.validation_failures + other.validation_failures,
            user_aborts: self.user_aborts + other.user_aborts,
        }
    }
}

impl Sub for TxStats {
    type Output = TxStats;
    /// The change between two readings: `after - before`.
    fn sub(self, before: TxStats) -> TxStats {
        TxStats {
            commits: self.commits - before.commits,
            read_conflicts: self.read_conflicts - before.read_conflicts,
            validation_failures: self.validation_failures - before.validation_failures,
            user_aborts: self.user_aborts - before.user_aborts,
        }
    }
}

impl std::iter::Sum for TxStats {
    fn sum<I: Iterator<Item = TxStats>>(iter: I) -> TxStats {
        iter.fold(TxStats::ZERO, Add::add)
    }
}

thread_local! {
    static STATS: Cell<TxStats> = const { Cell::new(TxStats::ZERO) };
}

/// The counters for every transaction attempted on the calling thread.
pub fn thread_stats() -> TxStats {
    STATS.with(Cell::get)
}

/// Set the calling thread's counters back to zero.
pub fn reset_thread_stats() {
    STATS.with(|stats| stats.set(TxStats::ZERO));
}

/// How one attempt ended. Used by the transaction layer to update counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Committed,
    ReadConflict,
    ValidationFailed,
    UserAbort,
}

/// Record one attempt's outcome on the calling thread.
pub(crate) fn record(outcome: Outcome) {
    STATS.with(|cell| {
        let mut stats = cell.get();
        match outcome {
            Outcome::Committed => stats.commits += 1,
            Outcome::ReadConflict => stats.read_conflicts += 1,
            Outcome::ValidationFailed => stats.validation_failures += 1,
            Outcome::UserAbort => stats.user_aborts += 1,
        }
        cell.set(stats);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_reset() {
        reset_thread_stats();
        record(Outcome::Committed);
        record(Outcome::Committed);
        record(Outcome::ReadConflict);
        record(Outcome::ValidationFailed);
        record(Outcome::UserAbort);

        let stats = thread_stats();
        assert_eq!(stats.commits, 2);
        assert_eq!(stats.conflicts(), 2);
        assert_eq!(stats.attempts(), 5);
        assert!((stats.conflict_rate() - 0.4).abs() < 1e-9);

        reset_thread_stats();
        assert_eq!(thread_stats(), TxStats::ZERO);
    }

    #[test]
    fn counters_are_per_thread() {
        reset_thread_stats();
        record(Outcome::Committed);
        let other = std::thread::spawn(|| {
            record(Outcome::ReadConflict);
            thread_stats()
        })
        .join()
        .unwrap();
        assert_eq!(other.read_conflicts, 1);
        assert_eq!(other.commits, 0);
        assert_eq!(thread_stats().commits, 1);
        assert_eq!(thread_stats().read_conflicts, 0);
    }

    #[test]
    fn arithmetic() {
        let a = TxStats {
            commits: 5,
            read_conflicts: 2,
            validation_failures: 1,
            user_aborts: 0,
        };
        let b = TxStats {
            commits: 1,
            ..TxStats::ZERO
        };
        assert_eq!((a + b) - b, a);
        assert_eq!([a, b].into_iter().sum::<TxStats>(), a + b);
        assert_eq!(TxStats::ZERO.conflict_rate(), 0.0);
    }
}
