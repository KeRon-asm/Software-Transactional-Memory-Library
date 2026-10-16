//! The transaction layer: `atomically`, read-set/write-set tracking, and
//! the commit protocol.
//!
//! This is the heart of the library. It depends on `clock.rs` (for read and
//! write versions) and `tvar.rs` (for the per-variable value, version and
//! commit lock).
//!
//! The shape of a transaction (the TL2 algorithm):
//!
//! 1. Snapshot the global clock as the transaction's "read version" `rv`.
//! 2. Run the user's closure. Every `TVar` it reads goes into a read-set.
//!    The read itself is checked against the snapshot: if the `TVar` was
//!    written after `rv` (or is mid-commit), the transaction aborts on the
//!    spot instead of continuing with inconsistent data. Every `TVar` it
//!    writes goes into a write-set (buffered — NOT applied to the real
//!    `TVar` yet).
//! 3. At commit (read-only transactions skip this: their reads were
//!    already validated one by one as they happened):
//!    - **Lock** every `TVar` in the write-set, in ascending id order — a
//!      FIXED GLOBAL ORDER, which prevents deadlock between two
//!      transactions committing at the same time.
//!    - **Bump** the global clock; the new value is the write version `wv`.
//!    - **Validate** the read-set: every `TVar` read must still have
//!      `version <= rv` and must not be locked by a different committer.
//!      (Skipped when `wv == rv + 1`, which proves nobody else committed
//!      since we started.) On failure, unlock and retry from step 1.
//!    - **Write back** every buffered value stamped with `wv`, then unlock.
//!
//! # Design decisions (details and reasoning in `ARCHITECTURE.md`)
//!
//! * **Read-set**: ids of the `TVar`s read, each with a type-erased handle.
//!   No per-entry version is stored: each read already enforced
//!   `version <= rv`, so commit-time validation re-checks that same bound.
//! * **Write-set**: a `BTreeMap` from `TVar` id to `(type-erased handle,
//!   boxed pending value)`. The map is keyed by id, so iterating it yields
//!   exactly the global lock order, and a transaction's own writes can be
//!   found again (read-your-writes) in `O(log n)`.
//! * **Errors vs. retry**: closures return `Result`. `Err(Conflict)` means
//!   "this attempt saw an inconsistent state, run me again" and is what
//!   `?` on [`Transaction::read`] produces. [`try_atomically`] additionally
//!   lets the closure fail with its own error, which aborts the transaction
//!   without retrying.
//! * **Retry**: a plain loop inside `atomically`. A transaction only retries
//!   because some other transaction committed (or is committing) a write it
//!   depends on, so the system as a whole always makes progress, but an
//!   individual transaction can starve under heavy contention. There is
//!   deliberately no backoff yet — a known gap, noted in `ARCHITECTURE.md`.

use std::any::Any;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;

use crate::clock;
use crate::tvar::{Cell, TVar};

/// "This attempt hit a conflict; run the transaction again."
///
/// Returned by [`Transaction::read`] when the value it would return is not
/// consistent with the transaction's snapshot. Closures normally just
/// propagate it with `?`, and `atomically` takes care of retrying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict;

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("transaction conflict (will be retried)")
    }
}

impl std::error::Error for Conflict {}

/// Why a [`try_atomically`] closure stopped early.
///
/// This is the answer to "how does a closure tell the library *retry me*
/// apart from *my own logic failed*": the first is [`TxError::Conflict`], the
/// second is [`TxError::Failed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxError<E> {
    /// A conflict with another transaction. `try_atomically` retries.
    Conflict,
    /// The closure's own failure (insufficient funds, invalid state, ...).
    /// Every buffered write is discarded and `try_atomically` returns
    /// `Err(E)` without retrying.
    Failed(E),
}

impl<E> From<Conflict> for TxError<E> {
    /// Lets `tx.read(&var)?` be used inside a `try_atomically` closure.
    fn from(_: Conflict) -> Self {
        TxError::Conflict
    }
}

/// A buffered write: the target `TVar` (type-erased) and the value to
/// install at commit, boxed so one map can hold writes of different types.
struct WriteEntry {
    cell: Arc<dyn Cell>,
    value: Box<dyn Any + Send>,
}

/// The context passed into a transaction's closure body.
///
/// This is what the user's code calls `.read()` and `.write()` on inside
/// an `atomically` block, instead of touching `TVar`s directly.
pub struct Transaction {
    /// Snapshot of the global clock at the start of this attempt.
    read_version: u64,
    /// `TVar`s read so far, by id. Order doesn't matter for validation.
    read_set: HashMap<u64, Arc<dyn Cell>>,
    /// Buffered writes, by `TVar` id. A `BTreeMap` so that iteration is in
    /// ascending id order, which is the global lock order for commit.
    write_set: BTreeMap<u64, WriteEntry>,
}

impl Transaction {
    fn new() -> Self {
        Transaction {
            read_version: clock::read_clock(),
            read_set: HashMap::new(),
            write_set: BTreeMap::new(),
        }
    }

    /// Read `var` as of this transaction's snapshot.
    ///
    /// * If this transaction already wrote `var`, returns that buffered
    ///   value (read-your-own-writes).
    /// * Otherwise returns the committed value and records the read so it
    ///   can be re-validated at commit.
    /// * If the committed value is newer than the snapshot, or a commit to
    ///   `var` is in flight, returns `Err(Conflict)`. Propagate it with `?`;
    ///   the transaction is re-run from the start. Aborting here, rather
    ///   than at commit, means the closure never computes with a mix of old
    ///   and new values.
    pub fn read<T>(&mut self, var: &TVar<T>) -> Result<T, Conflict>
    where
        T: Clone + Send + 'static,
    {
        if let Some(entry) = self.write_set.get(&var.id()) {
            let pending = entry
                .value
                .downcast_ref::<T>()
                .expect("write-set entry type must match its TVar's type");
            return Ok(pending.clone());
        }

        let value = var.read_at(self.read_version).ok_or(Conflict)?;
        self.read_set
            .entry(var.id())
            .or_insert_with(|| var.erased());
        Ok(value)
    }

    /// Buffer a write of `value` to `var`.
    ///
    /// Nothing is visible to other threads (or applied to `var`) until the
    /// transaction commits. Writing the same `TVar` twice keeps the last
    /// value.
    pub fn write<T>(&mut self, var: &TVar<T>, value: T)
    where
        T: Send + 'static,
    {
        self.write_set.insert(
            var.id(),
            WriteEntry {
                cell: var.erased(),
                value: Box::new(value),
            },
        );
    }

    /// Try to commit. Returns `true` if the transaction's effects are now
    /// visible, `false` if validation failed and the attempt must be redone.
    fn commit(&mut self) -> bool {
        // A read-only transaction has nothing to publish. Each of its reads
        // was already checked against `read_version`, so together they are a
        // consistent snapshot: no locks, no clock bump.
        if self.write_set.is_empty() {
            return true;
        }

        // (a) Lock the write-set in ascending id order. The guard unlocks
        // everything when dropped, so every early `return false` below
        // releases the locks.
        let locks = LockedSet::acquire(self.write_set.values().map(|e| Arc::clone(&e.cell)));

        // (b) Take the write version. This must come AFTER the locks are
        // held and BEFORE validation; see ARCHITECTURE.md for why.
        let write_version = clock::bump_clock();

        // (c) Validate the read-set. If `write_version == read_version + 1`
        // no other transaction bumped the clock since we started, so nothing
        // can have invalidated our reads and the checks can be skipped.
        if write_version != self.read_version + 1 {
            for (id, cell) in &self.read_set {
                let locked_by_us = self.write_set.contains_key(id);
                if !cell.validate(self.read_version, locked_by_us) {
                    return false;
                }
            }
        }

        // (d) Write back, stamping every value with the write version, then
        // release the locks (when `locks` drops).
        for (_, entry) in std::mem::take(&mut self.write_set) {
            entry.cell.store(entry.value, write_version);
        }
        drop(locks);
        true
    }
}

/// RAII guard over a set of locked `TVar`s: unlocks them all on drop, so a
/// failed validation (or a panic) can never leave a `TVar` locked forever.
struct LockedSet {
    cells: Vec<Arc<dyn Cell>>,
}

impl LockedSet {
    /// Lock `cells` in the order given. The caller must supply them in
    /// strictly ascending id order — that order is what rules out deadlock.
    fn acquire(cells: impl Iterator<Item = Arc<dyn Cell>>) -> Self {
        let mut set = LockedSet { cells: Vec::new() };
        for cell in cells {
            debug_assert!(
                set.cells.last().is_none_or(|prev| prev.id() < cell.id()),
                "locks must be taken in strictly ascending id order"
            );
            cell.lock();
            set.cells.push(cell);
        }
        set
    }
}

impl Drop for LockedSet {
    fn drop(&mut self) {
        for cell in self.cells.iter().rev() {
            cell.unlock();
        }
    }
}

/// The retry loop shared by [`atomically`] and [`try_atomically`].
fn run<F, T, E>(body: F) -> Result<T, E>
where
    F: Fn(&mut Transaction) -> Result<T, TxError<E>>,
{
    loop {
        let mut tx = Transaction::new();
        match body(&mut tx) {
            Ok(value) => {
                if tx.commit() {
                    return Ok(value);
                }
                // Validation failed: `tx` (both sets) is dropped, and we go
                // around again with a fresh snapshot. No backoff yet.
            }
            Err(TxError::Conflict) => {}
            // Every read in this attempt was checked against the snapshot,
            // so the closure's failure was decided on consistent data and
            // can be trusted. `tx` is dropped here, discarding its writes.
            Err(TxError::Failed(error)) => return Err(error),
        }
    }
}

/// Run `body` as an atomic transaction, retrying on conflict until it
/// commits successfully.
///
/// This is the only public entry point into the whole library for
/// ordinary use — everything else (`TVar`, `Transaction`) exists to
/// support this function.
///
/// `body` may run **more than once** (once per attempt), so it should only
/// interact with shared state through the `Transaction`, and avoid
/// irreversible side effects such as printing or sending messages. Use `?` on
/// [`Transaction::read`] to propagate conflicts.
///
/// ```
/// use stm::{atomically, TVar};
///
/// let balance = TVar::new(100);
/// atomically(|tx| {
///     let b = tx.read(&balance)?;
///     tx.write(&balance, b + 50);
///     Ok(())
/// });
/// assert_eq!(atomically(|tx| tx.read(&balance)), 150);
/// ```
pub fn atomically<F, T>(body: F) -> T
where
    F: Fn(&mut Transaction) -> Result<T, Conflict>,
{
    match run::<_, T, Infallible>(|tx| body(tx).map_err(|Conflict| TxError::Conflict)) {
        Ok(value) => value,
        // `Infallible` has no values, so this arm can never run.
        Err(never) => match never {},
    }
}

/// Like [`atomically`], but the closure can also fail with its own error.
///
/// Return `Err(TxError::Failed(e))` to abort the transaction: its buffered
/// writes are discarded, nothing is retried, and the call returns `Err(e)`.
/// `?` on [`Transaction::read`] still turns conflicts into retries.
///
/// ```
/// use stm::{TVar, transaction::{try_atomically, TxError}};
///
/// let balance = TVar::new(30);
/// let result = try_atomically(|tx| {
///     let b = tx.read(&balance)?;
///     if b < 50 {
///         return Err(TxError::Failed("insufficient funds"));
///     }
///     tx.write(&balance, b - 50);
///     Ok(())
/// });
/// assert_eq!(result, Err("insufficient funds"));
/// ```
pub fn try_atomically<F, T, E>(body: F) -> Result<T, E>
where
    F: Fn(&mut Transaction) -> Result<T, TxError<E>>,
{
    run(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    /// Commit a single blind write in its own transaction.
    fn set<T: Clone + Send + 'static>(var: &TVar<T>, value: T) {
        atomically(|tx| {
            tx.write(var, value.clone());
            Ok(())
        });
    }

    /// Read a `TVar` in its own (read-only) transaction.
    fn get<T: Clone + Send + 'static>(var: &TVar<T>) -> T {
        atomically(|tx| tx.read(var))
    }

    #[test]
    fn single_threaded_read_write_commit() {
        let var = TVar::new(1);
        assert_eq!(get(&var), 1);

        atomically(|tx| {
            let v = tx.read(&var)?;
            tx.write(&var, v + 41);
            Ok(())
        });

        assert_eq!(get(&var), 42);
    }

    #[test]
    fn writes_are_buffered_until_commit() {
        let var = TVar::new(1);
        let mut tx = Transaction::new();
        tx.write(&var, 2);

        // Not committed yet: the real TVar still holds the old value.
        assert_eq!(var.read_at(u64::MAX), Some(1));

        assert!(tx.commit());
        assert_eq!(var.read_at(u64::MAX), Some(2));
    }

    #[test]
    fn a_transaction_sees_its_own_writes() {
        let var = TVar::new(1);
        let seen = atomically(|tx| {
            tx.write(&var, 10);
            let first = tx.read(&var)?;
            tx.write(&var, first + 1);
            let second = tx.read(&var)?;
            Ok((first, second))
        });
        assert_eq!(seen, (10, 11));
        assert_eq!(get(&var), 11);
    }

    #[test]
    fn one_transaction_can_write_tvars_of_different_types() {
        let count = TVar::new(0u32);
        let label = TVar::new(String::from("old"));

        atomically(|tx| {
            tx.write(&count, 7);
            tx.write(&label, String::from("new"));
            Ok(())
        });

        assert_eq!(get(&count), 7);
        assert_eq!(get(&label), "new");
    }

    #[test]
    fn read_aborts_if_the_tvar_changed_after_the_snapshot() {
        let var = TVar::new(1);
        let mut stale = Transaction::new();

        // Someone else commits after `stale` took its snapshot...
        set(&var, 2);

        // ...so `stale` must refuse to read it rather than mix old and new.
        assert_eq!(stale.read(&var), Err(Conflict));
    }

    #[test]
    fn commit_fails_validation_if_a_read_was_overwritten() {
        let read_var = TVar::new(1);
        let write_var = TVar::new(0);

        let mut stale = Transaction::new();
        assert_eq!(stale.read(&read_var), Ok(1));
        stale.write(&write_var, 100);

        // A conflicting commit lands between our read and our commit.
        set(&read_var, 2);

        assert!(!stale.commit(), "validation must reject the stale read");

        // Nothing was written, and no lock was left behind.
        assert_eq!(write_var.read_at(u64::MAX), Some(0));
        assert_eq!(get(&read_var), 2);
    }

    #[test]
    fn conflicting_attempt_is_retried_with_a_fresh_snapshot() {
        let source = TVar::new(1);
        let copy = TVar::new(0);
        let attempts = AtomicU32::new(0);

        let result = atomically(|tx| {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
            let value = tx.read(&source)?;
            if attempt == 1 {
                // Simulate another thread committing after our read.
                set(&source, value + 10);
            }
            tx.write(&copy, value);
            Ok(value)
        });

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(result, 11, "the committed attempt used the fresh value");
        assert_eq!(get(&copy), 11);
    }

    #[test]
    fn failed_transaction_discards_its_writes() {
        let var = TVar::new(10);
        let result: Result<(), &str> = try_atomically(|tx| {
            tx.write(&var, 99);
            Err(TxError::Failed("nope"))
        });

        assert_eq!(result, Err("nope"));
        assert_eq!(get(&var), 10);
    }

    #[test]
    fn try_atomically_commits_on_success() {
        let var = TVar::new(10);
        let result: Result<i32, &str> = try_atomically(|tx| {
            let v = tx.read(&var)?;
            tx.write(&var, v * 2);
            Ok(v)
        });

        assert_eq!(result, Ok(10));
        assert_eq!(get(&var), 20);
    }

    #[test]
    fn concurrent_increments_lose_no_updates() {
        const THREADS: usize = 4;
        const INCREMENTS: usize = 2_000;

        let counter = TVar::new(0usize);
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let counter = counter.clone();
                thread::spawn(move || {
                    for _ in 0..INCREMENTS {
                        atomically(|tx| {
                            let n = tx.read(&counter)?;
                            tx.write(&counter, n + 1);
                            Ok(())
                        });
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(get(&counter), THREADS * INCREMENTS);
    }
}
