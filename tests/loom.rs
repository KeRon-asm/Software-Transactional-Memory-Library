//! Exhaustive interleaving tests of the commit protocol, using `loom`.
//!
//! An ordinary multi-threaded test only sees the thread schedules the OS
//! happens to pick. `loom` instead re-runs each test body under every
//! distinct interleaving of the atomic operations and mutex acquisitions
//! inside the library (up to a preemption bound), and models the C++11
//! memory model, so a bug that needs one specific, rare schedule is found
//! deterministically.
//!
//! These run against the real library code: `src/sync.rs` swaps `loom`'s
//! atomics and mutex in for the `std` / `parking_lot` ones when compiled with
//! `--cfg loom`. Run them with:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --release --test loom
//! ```
//!
//! Add `-- --nocapture` to see how many interleavings each test explored.
//!
//! Each test uses two threads, which is enough to exercise every pairwise
//! race in the protocol: read vs. commit, commit vs. commit, and
//! validation vs. commit. The search is bounded to at most four forced
//! preemptions per execution (see `DEFAULT_PREEMPTIONS`), which is
//! systematic but not a proof for every possible schedule.
//!
//! The atomics in `src/` use acquire/release ordering, not `SeqCst`. loom
//! models acquire/release precisely (it treats `SeqCst` the same way), so a
//! pass here also checks that the weaker orderings are enough.

#![cfg(loom)]

use loom::thread;
use stm::{TVar, atomically};

/// Default preemption bound: at most this many forced context switches per
/// execution. Bugs found in practice almost always need very few
/// preemptions; the bound keeps the search systematic and tractable.
///
/// Note: loom's *unbounded* mode is not a stronger check here. It uses a
/// partial-order reduction that, for these tests, explored far fewer
/// executions and missed a planted bug (readers ignoring the commit lock)
/// that bounds 2, 3 and 4 all catch. Override with `STM_LOOM_PREEMPTIONS=n`.
const DEFAULT_PREEMPTIONS: usize = 4;

fn model<F: Fn() + Sync + Send + 'static>(test: F) {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(
        std::env::var("STM_LOOM_PREEMPTIONS")
            .ok()
            .and_then(|n| n.parse().ok())
            .unwrap_or(DEFAULT_PREEMPTIONS),
    );

    // Count executions so the test output shows how much was explored.
    let executions = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&executions);
    builder.check(move || {
        counter.fetch_add(1, Ordering::Relaxed);
        test();
    });
    let bound = builder
        .preemption_bound
        .map_or("none".to_string(), |n| n.to_string());
    eprintln!(
        "explored {} interleavings (preemption bound: {bound})",
        executions.load(Ordering::Relaxed)
    );
}

fn transfer(from: &TVar<i64>, to: &TVar<i64>, amount: i64) {
    atomically(|tx| {
        let a = tx.read(from)?;
        let b = tx.read(to)?;
        tx.write(from, a - amount);
        tx.write(to, b + amount);
        Ok(())
    });
}

fn get(var: &TVar<i64>) -> i64 {
    atomically(|tx| tx.read(var))
}

/// Two transfers in OPPOSITE directions over the same pair of accounts.
/// With unordered locking this is the textbook deadlock; with lost updates
/// the total would drift. Neither may happen in any interleaving.
#[test]
fn opposite_transfers_neither_deadlock_nor_lose_money() {
    model(|| {
        let a = TVar::new(10);
        let b = TVar::new(10);

        let (a2, b2) = (a.clone(), b.clone());
        let other = thread::spawn(move || transfer(&a2, &b2, 3));
        transfer(&b, &a, 4);
        other.join().unwrap();

        assert_eq!(get(&a), 10 - 3 + 4);
        assert_eq!(get(&b), 10 + 3 - 4);
    });
}

/// A read-only transaction running concurrently with a transfer must see
/// either the state before it or the state after it, never "money in
/// flight". This is the race that the lock-flag check in `TVar::read_at`
/// and the SeqCst ordering argument in ARCHITECTURE.md exist to close.
#[test]
fn reader_never_observes_a_partial_transfer() {
    model(|| {
        let a = TVar::new(10);
        let b = TVar::new(10);

        let (a2, b2) = (a.clone(), b.clone());
        let writer = thread::spawn(move || transfer(&a2, &b2, 5));

        let (seen_a, seen_b) = atomically(|tx| Ok((tx.read(&a)?, tx.read(&b)?)));
        assert_eq!(seen_a + seen_b, 20, "saw a={seen_a} b={seen_b}");
        assert!(
            (seen_a, seen_b) == (10, 10) || (seen_a, seen_b) == (5, 15),
            "snapshot is neither before nor after the transfer: a={seen_a} b={seen_b}"
        );

        writer.join().unwrap();
    });
}

/// Two read-modify-write increments of one counter: the final value must
/// reflect both, in every interleaving.
#[test]
fn concurrent_increments_are_not_lost() {
    model(|| {
        let counter = TVar::new(0);
        let increment = |c: &TVar<i64>| {
            atomically(|tx| {
                let n = tx.read(c)?;
                tx.write(c, n + 1);
                Ok(())
            })
        };

        let c2 = counter.clone();
        let other = thread::spawn(move || increment(&c2));
        increment(&counter);
        other.join().unwrap();

        assert_eq!(get(&counter), 2);
    });
}

/// Write skew: each transaction reads BOTH variables but writes only ONE.
/// Snapshot isolation alone would let both commit, zeroing both. TL2
/// validates the read-set at commit, so the transactions are serialisable
/// and at most one of them can see "both are 1" and act on it.
#[test]
fn write_skew_is_prevented() {
    model(|| {
        let x = TVar::new(1);
        let y = TVar::new(1);

        // "Take one off duty, but only if both are on duty."
        let off_duty = |me: &TVar<i64>, other: &TVar<i64>| {
            atomically(|tx| {
                let mine = tx.read(me)?;
                let theirs = tx.read(other)?;
                if mine + theirs == 2 {
                    tx.write(me, 0);
                }
                Ok(())
            })
        };

        let (x2, y2) = (x.clone(), y.clone());
        let other = thread::spawn(move || off_duty(&y2, &x2));
        off_duty(&x, &y);
        other.join().unwrap();

        let (fx, fy) = (get(&x), get(&y));
        assert_eq!(fx + fy, 1, "exactly one may go off duty: x={fx} y={fy}");
    });
}
