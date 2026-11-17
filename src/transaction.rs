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
//!    writes goes into a write-set (buffered, NOT applied to the real
//!    `TVar` yet).
//! 3. At commit (read-only transactions skip this: their reads were
//!    already validated one by one as they happened):
//!    - **Lock** every `TVar` in the write-set, in ascending id order. This
//!      FIXED GLOBAL ORDER prevents deadlock between two transactions
//!      committing at the same time.
//!    - **Bump** the global clock; the new value is the write version `wv`.
//!    - **Validate** the read-set: every `TVar` read must still have
//!      `version <= rv` and must not be locked by a different committer.
//!      (Skipped when `wv == rv + 1`, which proves nobody else committed
//!      since we started.) On failure, unlock and retry from step 1.
//!    - **Write back** every buffered value stamped with `wv`, then unlock.
//!
//! # Design decisions (details and reasoning in `ARCHITECTURE.md`)
//!
//! * **Read-set**: a small inline vector of `(id, type-erased handle)`.
//!   No per-entry version is stored: each read already enforced
//!   `version <= rv`, so commit-time validation re-checks that same bound.
//!   Reading the same `TVar` twice adds a duplicate entry, which costs one
//!   extra (harmless) validation and avoids a lookup on every read.
//! * **Write-set**: a small inline vector of `(id, handle, boxed value)`,
//!   with at most one entry per `TVar`. It is sorted by id only once, at
//!   commit, which gives the global lock order. Transactions are usually
//!   small, so the linear scan for read-your-own-writes is cheaper than a
//!   map. (The first version used a `HashMap` and a `BTreeMap`; switching to
//!   inline vectors removed two heap allocations per transaction.)
//! * **Errors vs. retry**: closures return `Result`. `Err(Conflict)` means
//!   "this attempt saw an inconsistent state, run me again" and is what
//!   `?` on [`Transaction::read`] produces. [`try_atomically`] additionally
//!   lets the closure fail with its own error, which aborts the transaction
//!   without retrying.
//! * **Retry**: a loop inside `atomically` with bounded exponential backoff
//!   between attempts (see [`Backoff`]). A transaction only retries because
//!   some other transaction committed (or is committing) a write it depends
//!   on, so the system as a whole always makes progress; backoff reduces how
//!   often colliding transactions keep colliding.

use smallvec::SmallVec;
use std::any::Any;
use std::convert::Infallible;
use std::fmt;

use crate::clock;
use crate::stats::{self, Outcome};
use crate::sync;
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
/// install at commit, boxed so one set can hold writes of different types.
struct WriteEntry<'env> {
    id: u64,
    cell: &'env dyn Cell,
    /// Always `Some` until write-back moves the value into the `TVar`.
    value: Option<Box<dyn Any + Send>>,
}

/// Inline capacity of the read-set before it spills to the heap.
const READ_INLINE: usize = 8;
/// Inline capacity of the write-set before it spills to the heap.
const WRITE_INLINE: usize = 4;

type ReadSet<'env> = SmallVec<[(u64, &'env dyn Cell); READ_INLINE]>;
type WriteSet<'env> = SmallVec<[WriteEntry<'env>; WRITE_INLINE]>;

/// The context passed into a transaction's closure body.
///
/// This is what the user's code calls `.read()` and `.write()` on inside
/// an `atomically` block, instead of touching `TVar`s directly.
///
/// The lifetime `'env` is how long the `TVar`s it touches are borrowed for.
/// The read-set and write-set hold plain `&'env` references to them rather
/// than reference-counted handles, so recording a read or a write costs no
/// atomic operation, and the compiler guarantees every `TVar` outlives the
/// transaction that refers to it. For example, this does not compile,
/// because the `TVar` would be dropped while the read-set still points at it:
///
/// ```compile_fail
/// use stm::{atomically, TVar};
///
/// atomically(|tx| {
///     let temporary = TVar::new(1);
///     tx.read(&temporary)
/// });
/// ```
///
/// A helper that takes a transaction and some `TVar`s names the shared
/// lifetime explicitly:
///
/// ```
/// use stm::{atomically, Conflict, TVar, Transaction};
///
/// fn deposit<'env>(tx: &mut Transaction<'env>, account: &'env TVar<i64>, amount: i64)
///     -> Result<(), Conflict>
/// {
///     let balance = tx.read(account)?;
///     tx.write(account, balance + amount);
///     Ok(())
/// }
///
/// let account = TVar::new(10);
/// atomically(|tx| deposit(tx, &account, 5));
/// assert_eq!(atomically(|tx| tx.read(&account)), 15);
/// ```
pub struct Transaction<'env> {
    /// Snapshot of the global clock at the start of this attempt.
    read_version: u64,
    /// `TVar`s read so far (possibly with duplicates). Order doesn't matter.
    read_set: ReadSet<'env>,
    /// Buffered writes, at most one per `TVar`. Sorted by id at commit.
    write_set: WriteSet<'env>,
}

impl<'env> Transaction<'env> {
    fn new() -> Self {
        Transaction {
            read_version: clock::read_clock(),
            read_set: SmallVec::new(),
            write_set: SmallVec::new(),
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
    pub fn read<T>(&mut self, var: &'env TVar<T>) -> Result<T, Conflict>
    where
        T: Clone + Send + 'static,
    {
        let id = var.id();
        if let Some(entry) = self.write_set.iter().find(|e| e.id == id) {
            let pending = entry
                .value
                .as_ref()
                .and_then(|v| v.downcast_ref::<T>())
                .expect("write-set entry type must match its TVar's type");
            return Ok(pending.clone());
        }

        let value = var.read_at(self.read_version).ok_or(Conflict)?;
        self.read_set.push((id, var.cell()));
        Ok(value)
    }

    /// Buffer a write of `value` to `var`.
    ///
    /// Nothing is visible to other threads (or applied to `var`) until the
    /// transaction commits. Writing the same `TVar` twice keeps the last
    /// value.
    pub fn write<T>(&mut self, var: &'env TVar<T>, value: T)
    where
        T: Send + 'static,
    {
        let id = var.id();
        let value: Box<dyn Any + Send> = Box::new(value);
        match self.write_set.iter_mut().find(|e| e.id == id) {
            Some(entry) => entry.value = Some(value),
            None => self.write_set.push(WriteEntry {
                id,
                cell: var.cell(),
                value: Some(value),
            }),
        }
    }

    /// Number of entries in the read-set (including duplicates).
    #[cfg_attr(not(feature = "trace"), allow(dead_code))]
    fn reads(&self) -> usize {
        self.read_set.len()
    }

    /// Number of distinct `TVar`s in the write-set.
    fn writes(&self) -> usize {
        self.write_set.len()
    }

    /// Try to commit. Returns `true` if the transaction's effects are now
    /// visible, `false` if validation failed and the attempt must be redone.
    /// Either way the write-set is consumed.
    fn commit(&mut self) -> bool {
        // A read-only transaction has nothing to publish. Each of its reads
        // was already checked against `read_version`, so together they are a
        // consistent snapshot: no locks, no clock bump.
        if self.write_set.is_empty() {
            return true;
        }

        // (a) Sort the write-set into the global lock order and lock it. The
        // guard owns the entries and unlocks them when dropped, so every
        // early `return false` below releases the locks.
        let mut entries = std::mem::take(&mut self.write_set);
        entries.sort_unstable_by_key(|e| e.id);
        let mut locked = LockedSet::acquire(entries);

        // (b) Take the write version. This must come AFTER the locks are
        // held and BEFORE validation; see ARCHITECTURE.md for why.
        let write_version = clock::bump_clock();

        // (c) Validate the read-set. If `write_version == read_version + 1`
        // no other transaction bumped the clock since we started, so nothing
        // can have invalidated our reads and the checks can be skipped.
        if write_version != self.read_version + 1 {
            for (id, cell) in &self.read_set {
                if !cell.validate(self.read_version, locked.contains(*id)) {
                    return false;
                }
            }
        }

        // (d) Write back, stamping every value with the write version, then
        // release the locks (when `locked` drops).
        locked.write_back(write_version);
        true
    }
}

/// RAII guard over a write-set whose `TVar`s are locked: unlocks every
/// locked entry on drop, so a failed validation (or a panic) can never leave
/// a `TVar` locked forever.
struct LockedSet<'env> {
    entries: WriteSet<'env>,
    /// How many entries (a prefix of `entries`) are currently locked.
    locked: usize,
}

impl<'env> LockedSet<'env> {
    /// Lock every entry, in order. The entries must be sorted by strictly
    /// ascending id; that order is what rules out deadlock.
    fn acquire(entries: WriteSet<'env>) -> Self {
        let mut set = LockedSet { entries, locked: 0 };
        for i in 0..set.entries.len() {
            debug_assert!(
                i == 0 || set.entries[i - 1].id < set.entries[i].id,
                "locks must be taken in strictly ascending id order"
            );
            set.entries[i].cell.lock();
            set.locked = i + 1;
        }
        set
    }

    /// Is the `TVar` with this id one of ours (and therefore locked by us)?
    fn contains(&self, id: u64) -> bool {
        self.entries.binary_search_by_key(&id, |e| e.id).is_ok()
    }

    /// Install every buffered value, stamped with `version`.
    fn write_back(&mut self, version: u64) {
        for entry in &mut self.entries {
            let value = entry.value.take().expect("each value is written once");
            entry.cell.store(value, version);
        }
    }
}

impl Drop for LockedSet<'_> {
    fn drop(&mut self) {
        for entry in self.entries[..self.locked].iter().rev() {
            entry.cell.unlock();
        }
    }
}

/// Bounded exponential backoff between retries.
///
/// After the k-th consecutive failed attempt, spin for about `2^k` pause
/// instructions (k capped at [`Backoff::SPIN_LIMIT`]); beyond that, yield
/// the thread instead. Two transactions that collided once are likely to
/// collide again if they retry immediately, so waiting a little, and a
/// little longer each time, lets one of them finish first.
struct Backoff {
    step: u32,
}

impl Backoff {
    const SPIN_LIMIT: u32 = 6;
    const YIELD_LIMIT: u32 = 10;

    fn new() -> Self {
        Backoff { step: 0 }
    }

    fn snooze(&mut self) {
        if self.step <= Self::SPIN_LIMIT {
            for _ in 0..(1u32 << self.step) {
                sync::spin_loop();
            }
        } else {
            sync::yield_now();
        }
        if self.step < Self::YIELD_LIMIT {
            self.step += 1;
        }
    }
}

/// Log one attempt to stderr when the `trace` feature is on.
#[cfg(feature = "trace")]
fn trace(attempt: u32, tx: &Transaction<'_>, writes: usize, outcome: Outcome) {
    eprintln!(
        "[stm] {:?} attempt={attempt} rv={} reads={} writes={writes} outcome={outcome:?}",
        std::thread::current().id(),
        tx.read_version,
        tx.reads(),
    );
}

#[cfg(not(feature = "trace"))]
#[inline(always)]
fn trace(_attempt: u32, _tx: &Transaction<'_>, _writes: usize, _outcome: Outcome) {}

/// The retry loop shared by [`atomically`] and [`try_atomically`].
fn run<'env, F, T, E>(body: F) -> Result<T, E>
where
    F: Fn(&mut Transaction<'env>) -> Result<T, TxError<E>>,
{
    let mut backoff = Backoff::new();
    let mut attempt = 0u32;
    loop {
        attempt = attempt.wrapping_add(1);
        let mut tx = Transaction::new();
        let outcome = match body(&mut tx) {
            Ok(value) => {
                let writes = tx.writes();
                if tx.commit() {
                    stats::record(Outcome::Committed);
                    trace(attempt, &tx, writes, Outcome::Committed);
                    return Ok(value);
                }
                // Validation failed: `tx` (both sets) is dropped, and we go
                // around again with a fresh snapshot.
                trace(attempt, &tx, writes, Outcome::ValidationFailed);
                Outcome::ValidationFailed
            }
            Err(TxError::Conflict) => {
                trace(attempt, &tx, tx.writes(), Outcome::ReadConflict);
                Outcome::ReadConflict
            }
            // Every read in this attempt was checked against the snapshot,
            // so the closure's failure was decided on consistent data and
            // can be trusted. `tx` is dropped here, discarding its writes.
            Err(TxError::Failed(error)) => {
                stats::record(Outcome::UserAbort);
                trace(attempt, &tx, tx.writes(), Outcome::UserAbort);
                return Err(error);
            }
        };
        stats::record(outcome);
        backoff.snooze();
    }
}

/// Run `body` as an atomic transaction, retrying on conflict until it
/// commits successfully.
///
/// This is the only public entry point into the whole library for
/// ordinary use; everything else (`TVar`, `Transaction`) exists to
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
pub fn atomically<'env, F, T>(body: F) -> T
where
    F: Fn(&mut Transaction<'env>) -> Result<T, Conflict>,
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
pub fn try_atomically<'env, F, T, E>(body: F) -> Result<T, E>
where
    F: Fn(&mut Transaction<'env>) -> Result<T, TxError<E>>,
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

    #[test]
    fn writing_a_tvar_twice_keeps_the_last_value() {
        let var = TVar::new(0);
        atomically(|tx| {
            tx.write(&var, 1);
            tx.write(&var, 2);
            Ok(())
        });
        assert_eq!(get(&var), 2);
    }

    #[test]
    fn large_transactions_spill_past_the_inline_capacity() {
        // More TVars than the inline read-set and write-set capacity, to
        // exercise the heap-allocated path and the sort before locking.
        let vars: Vec<TVar<u32>> = (0..50).map(TVar::new).collect();
        atomically(|tx| {
            for var in vars.iter().rev() {
                let v = tx.read(var)?;
                tx.write(var, v * 2);
            }
            Ok(())
        });
        let total: u32 = atomically(|tx| vars.iter().map(|v| tx.read(v)).sum());
        assert_eq!(total, (0..50).map(|v| v * 2).sum());
    }

    #[test]
    fn stats_count_each_outcome() {
        crate::stats::reset_thread_stats();
        let var = TVar::new(0);

        // One clean commit.
        set(&var, 1);

        // One read conflict then a commit: the closure writes `var` from
        // "another transaction" before reading it on the first attempt.
        let first = AtomicU32::new(0);
        atomically(|tx| {
            if first.fetch_add(1, Ordering::SeqCst) == 0 {
                set(&var, 5); // counts as its own commit
            }
            let v = tx.read(&var)?;
            tx.write(&var, v + 1);
            Ok(())
        });

        // One user abort.
        let _: Result<(), ()> = try_atomically(|_| Err(TxError::Failed(())));

        let stats = crate::stats::thread_stats();
        // set(1) + inner set(5) + the retried outer transaction = 3 commits.
        assert_eq!(stats.commits, 3);
        assert_eq!(stats.read_conflicts, 1);
        assert_eq!(stats.validation_failures, 0);
        assert_eq!(stats.user_aborts, 1);
    }

    #[test]
    fn backoff_grows_then_caps() {
        let mut backoff = Backoff::new();
        for _ in 0..50 {
            backoff.snooze();
        }
        assert_eq!(backoff.step, Backoff::YIELD_LIMIT);
    }
}
