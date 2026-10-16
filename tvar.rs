//! `TVar<T>`: a transactional variable.
//!
//! A `TVar` is the only place shared, mutable state lives in this library.
//! Code never touches a `TVar`'s value directly — it only reads and writes
//! through a transaction (see `transaction.rs`). Each `TVar` carries:
//!
//!   1. The current value (protected so it can be safely mutated from
//!      multiple threads).
//!   2. A version number: the value of the global clock (see `clock.rs`)
//!      at the moment this `TVar` was last successfully written.
//!
//! The version is what the commit protocol checks during validation: if a
//! transaction read a `TVar` at version N, and by commit time the `TVar`'s
//! version is still N, nobody else wrote to it in the meantime.
//!
//! # Locking design
//!
//! Each `TVar` has two separate pieces of synchronisation, because they do
//! two different jobs:
//!
//! * `cell: Mutex<Inner<T>>` protects the value and its version **together**.
//!   It is only ever held for a few instructions (clone the value, or swap in
//!   a new one), so a reader can never observe a value paired with the
//!   version of a different write. This is the "one lock for both" option
//!   from the design notes: it rules out the torn value/version read that a
//!   separate `AtomicU64` version would allow.
//!
//! * `commit_lock: AtomicBool` is the long-lived lock a committing
//!   transaction holds across the whole commit protocol (lock → validate →
//!   write back → unlock). It cannot be a `MutexGuard`, because a commit
//!   holds locks on `TVar`s of *different* `T` at once and has to carry them
//!   through several steps; a plain flag can be taken and released through
//!   the type-erased `Cell` interface below. Readers never wait on it: a
//!   reader that sees it set gives up and the transaction retries.
//!
//! # Type erasure
//!
//! A transaction's write-set holds writes to TVars of many different types.
//! `Cell` is the object-safe slice of a `TVar` that the commit protocol
//! needs (lock, validate, store, unlock), so the write-set can hold
//! `Arc<dyn Cell>` regardless of `T`.

use parking_lot::Mutex;
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Source of unique TVar ids. Only uniqueness matters here, so `Relaxed`
/// is enough: no other memory is published through this counter.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// A transactional variable holding a value of type `T` plus a version.
///
/// Cloning a `TVar` gives you another handle to the SAME underlying data
/// (think `Arc<Mutex<...>>`, not a deep copy) — that's what lets multiple
/// threads and multiple transactions refer to the same shared cell.
pub struct TVar<T> {
    shared: Arc<Shared<T>>,
}

/// Everything behind the `Arc`: identity, the commit-phase lock, and the
/// value/version pair.
struct Shared<T> {
    /// Unique, never-changing id. Committing transactions lock their
    /// write-set in ascending `id` order, which is the "fixed global order"
    /// that prevents deadlock. (An id is used rather than the pointer
    /// address so the ordering is explicit and doesn't depend on allocator
    /// behaviour.)
    id: u64,
    /// Held by a committing transaction from "lock write-set" until
    /// "write back" is finished. See the module docs.
    commit_lock: AtomicBool,
    /// The value and its version, always read and written together.
    cell: Mutex<Inner<T>>,
}

/// The data actually protected by the lock: the value and its version,
/// kept together so a reader always sees a value and its matching
/// version from the same point in time.
struct Inner<T> {
    value: T,
    version: u64,
}

impl<T> TVar<T> {
    /// Create a new `TVar` holding `value`, with an initial version.
    ///
    /// The initial version is 0. A transaction's read version is a clock
    /// reading, which is never below 0, so a freshly created `TVar` always
    /// passes the "version <= my read version" check: version 0 means
    /// "older than every transaction". (There is no earlier history that a
    /// transaction could be inconsistent with.)
    pub fn new(value: T) -> Self {
        TVar {
            shared: Arc::new(Shared {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                commit_lock: AtomicBool::new(false),
                cell: Mutex::new(Inner { value, version: 0 }),
            }),
        }
    }

    /// The unique id of this `TVar` (shared by all its clones).
    ///
    /// A transaction uses it as the key for its read-set and write-set, and
    /// the write-set's ascending-id iteration order is the global lock order.
    pub(crate) fn id(&self) -> u64 {
        self.shared.id
    }

    /// Read the value as of a transaction's snapshot.
    ///
    /// Returns `None` — meaning "this transaction can no longer see a
    /// consistent picture, abort and retry" — if either:
    ///
    /// * a committing transaction currently holds this `TVar`'s commit lock
    ///   (it may be about to write a version at or below our snapshot, so
    ///   what we would read could be stale), or
    /// * the `TVar` was written after the transaction's snapshot
    ///   (`version > read_version`).
    ///
    /// The lock flag is checked *before* taking the value, and the value and
    /// version are then read together under the mutex. `ARCHITECTURE.md`
    /// explains why that order is sufficient.
    pub(crate) fn read_at(&self, read_version: u64) -> Option<T>
    where
        T: Clone,
    {
        if self.shared.commit_lock.load(Ordering::SeqCst) {
            return None;
        }
        let inner = self.shared.cell.lock();
        if inner.version > read_version {
            return None;
        }
        Some(inner.value.clone())
    }
}

impl<T: Send + 'static> TVar<T> {
    /// A type-erased handle to this `TVar`, for storing in a transaction's
    /// read-set or write-set regardless of `T`.
    pub(crate) fn erased(&self) -> Arc<dyn Cell> {
        Arc::clone(&self.shared) as Arc<dyn Cell>
    }
}

impl<T> Clone for TVar<T> {
    fn clone(&self) -> Self {
        TVar {
            shared: Arc::clone(&self.shared),
        }
    }
}

/// The type-independent operations the commit protocol needs from a `TVar`.
///
/// Implemented for the shared state behind every `TVar<T>`; used through
/// `Arc<dyn Cell>` so one write-set can mix `TVar`s of different types.
pub(crate) trait Cell: Send + Sync {
    /// The `TVar`'s unique id (the lock-ordering key).
    fn id(&self) -> u64;

    /// Acquire the commit lock, waiting if another committer holds it.
    ///
    /// Waiting cannot deadlock as long as every committer takes its locks in
    /// ascending id order, because then no cycle of "A waits for B waits
    /// for A" can form. Must be paired with exactly one [`Cell::unlock`].
    fn lock(&self);

    /// Release the commit lock taken by [`Cell::lock`].
    fn unlock(&self);

    /// Commit-time validation of one read-set entry.
    ///
    /// Returns `true` if nothing has changed this `TVar` since the
    /// transaction's snapshot: its version is still `<= read_version`, and
    /// no *other* committer holds its lock. `locked_by_us` says the calling
    /// transaction holds the lock itself (the `TVar` is also in its
    /// write-set), in which case a set lock flag is expected and ignored.
    fn validate(&self, read_version: u64, locked_by_us: bool) -> bool;

    /// Install `value` and stamp it with `version`. The caller must hold
    /// the commit lock. `value` must be a `Box<T>` for this `TVar`'s `T`.
    fn store(&self, value: Box<dyn Any + Send>, version: u64);
}

impl<T: Send + 'static> Cell for Shared<T> {
    fn id(&self) -> u64 {
        self.id
    }

    fn lock(&self) {
        let mut spins = 0u32;
        while self
            .commit_lock
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            // Wait until it looks free before trying the compare-exchange
            // again, so waiters don't hammer the cache line. The critical
            // section is short (a handful of stores), so spin briefly and
            // then yield in case the holder was descheduled.
            while self.commit_lock.load(Ordering::SeqCst) {
                spins = spins.saturating_add(1);
                if spins < 100 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }

    fn unlock(&self) {
        self.commit_lock.store(false, Ordering::SeqCst);
    }

    fn validate(&self, read_version: u64, locked_by_us: bool) -> bool {
        if !locked_by_us && self.commit_lock.load(Ordering::SeqCst) {
            return false;
        }
        self.cell.lock().version <= read_version
    }

    fn store(&self, value: Box<dyn Any + Send>, version: u64) {
        let value = *value
            .downcast::<T>()
            .expect("write-set entry type must match its TVar's type");
        let mut inner = self.cell.lock();
        inner.value = value;
        inner.version = version;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::thread;

    #[test]
    fn new_tvar_holds_its_value() {
        let var = TVar::new(42);
        assert_eq!(var.read_at(u64::MAX), Some(42));
        // Initial version is 0, so even the oldest possible snapshot sees it.
        assert_eq!(var.read_at(0), Some(42));
    }

    #[test]
    fn clones_share_the_same_cell() {
        let a = TVar::new(String::from("before"));
        let b = a.clone();
        assert_eq!(a.id(), b.id());

        a.erased().lock();
        a.erased().store(Box::new(String::from("after")), 1);
        a.erased().unlock();

        // Written through `a`, visible through `b`.
        assert_eq!(b.read_at(1), Some(String::from("after")));
    }

    #[test]
    fn ids_are_unique() {
        let a = TVar::new(0u8);
        let b = TVar::new(0u8);
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn read_at_rejects_values_newer_than_the_snapshot() {
        let var = TVar::new(1);
        let cell = var.erased();
        cell.lock();
        cell.store(Box::new(2), 5);
        cell.unlock();

        assert_eq!(var.read_at(4), None, "written at 5, snapshot at 4");
        assert_eq!(var.read_at(5), Some(2));
        assert_eq!(var.read_at(9), Some(2));
    }

    #[test]
    fn read_at_rejects_a_tvar_locked_by_a_committer() {
        let var = TVar::new(1);
        let cell = var.erased();

        cell.lock();
        assert_eq!(var.read_at(u64::MAX), None);
        cell.unlock();
        assert_eq!(var.read_at(u64::MAX), Some(1));
    }

    #[test]
    fn validate_checks_version_and_foreign_locks() {
        let var = TVar::new(1);
        let cell = var.erased();
        cell.lock();
        cell.store(Box::new(2), 5);
        cell.unlock();

        assert!(cell.validate(5, false));
        assert!(cell.validate(7, false));
        assert!(
            !cell.validate(4, false),
            "version 5 is newer than snapshot 4"
        );

        // Locked by someone else: invalid. Locked by us: the flag is ignored.
        cell.lock();
        assert!(!cell.validate(5, false));
        assert!(cell.validate(5, true));
        cell.unlock();
    }

    #[test]
    fn commit_lock_is_mutually_exclusive() {
        const THREADS: usize = 4;
        const ROUNDS: usize = 5_000;

        let var = TVar::new(());
        let inside = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let cell = var.erased();
                let inside = Arc::clone(&inside);
                thread::spawn(move || {
                    for _ in 0..ROUNDS {
                        cell.lock();
                        // If the lock works, nobody else is in here.
                        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0);
                        std::hint::spin_loop();
                        inside.fetch_sub(1, Ordering::SeqCst);
                        cell.unlock();
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }
    }
}
