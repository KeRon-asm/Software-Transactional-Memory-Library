# Architecture

Design-decision notes for the STM library, which implements the TL2 algorithm
(Dice, Shalev & Shavit, "Transactional Locking II"). This document is graded as
evidence of understanding (interim report's "challenges encountered," final
report's "description of work completed") and must get into the interim-report
PDF.

> **Note for the author:** these notes record the reasoning behind the
> implementation as it currently stands. Read each section against the code and
> rewrite anything in your own words before you submit; you should be able to
> defend every decision below in a viva.

## Overview

```
atomically(|tx| { ... })
   │
   ├─ run loop (transaction.rs)          retries until a commit succeeds
   │    ├─ Transaction::new()            rv = clock::read_clock()
   │    ├─ body(&mut tx)                 tx.read / tx.write
   │    └─ tx.commit()                   lock → bump clock → validate → write back → unlock
   │
   ├─ TVar<T>  (tvar.rs)                 value + version under a Mutex, plus a commit lock flag
   └─ Clock    (clock.rs)                one global AtomicU64
```

## Resolved design questions

### `TVar<T>` internal locking

**Chosen:** one `parking_lot::Mutex<Inner<T>>` holding `value` and `version`
together, **plus** a separate `commit_lock: AtomicBool`.

*Why the value and version share one mutex.* The alternative was a separate
`AtomicU64` version next to a `Mutex<T>`. That lets a reader see a version and a
value from two different moments: it reads version 5, a writer then installs
value B at version 9, and the reader reads value B. The reader now believes it
holds "the value as of version 5" and passes the `version <= rv` check while
actually holding a value from the future, which silently breaks the consistent
snapshot the whole algorithm depends on. A seqlock-style double read of the
version could repair this, but it is subtle to get right. Putting both under one
mutex makes the pair impossible to tear, and the mutex is only ever held for a
clone or a swap, so contention on it is negligible.

*Why there is also a separate `commit_lock` flag.* A committing transaction has
to hold locks on `TVar`s of many different `T` across several protocol steps
(lock, bump, validate, write back). A `MutexGuard<'_, Inner<T>>` is typed by `T`
and borrows from its `TVar`, so it can't be stored in one heterogeneous
collection and carried through those steps. A plain flag can be set and cleared
through a type-erased interface (see "Type erasure" below). Readers never block
on the flag: they check it, and if it is set the transaction aborts and retries.

*Cost.* Every read takes the mutex briefly, and validation takes it to read the
version. A cheaper design would mirror the version in an `AtomicU64` for
validation only. Not done yet; noted as a possible optimisation.

### Transaction identity for lock ordering

**Chosen:** a unique `id: u64` assigned in `TVar::new` from a global
`AtomicU64` counter. All clones of a `TVar` share the same `Arc`, so they share
one id.

The write-set is a `BTreeMap<u64, WriteEntry>` keyed by that id, so iterating it
yields the entries in ascending id order. That iteration order **is** the fixed
global lock order. If every committer acquires locks in ascending id order, no
cycle of "A waits for B, B waits for A" can form, so commits cannot deadlock.
`LockedSet::acquire` has a `debug_assert!` that the order really is strictly
ascending.

An id was chosen over the heap address of the `Arc` because it makes the
ordering explicit and independent of allocator behaviour. (The address would
also be stable while the `TVar` is alive, so it would have worked.)

### Memory ordering

**Chosen:** `Ordering::SeqCst` for the clock and for every operation on the
commit-lock flag. `Relaxed` only for the id counter, which just needs
uniqueness and publishes no other memory.

*Why not weaker.* The correctness argument needs a single total order across two
different atomics (the clock and the lock flag). Consider a committer C that
will write at version `wv`, and a reader R that starts with snapshot `rv >= wv`:

1. C sets its lock flag, *then* bumps the clock to `wv`.
2. R reads the clock (sees `rv >= wv`), *then* checks C's lock flag.

In the `SeqCst` total order, C's lock precedes C's bump, which precedes R's
clock read, which precedes R's flag check. So R's flag check must see C's lock
or something later: either the flag is still set (R aborts, correct, because the
value R would read is stale), or C has already unlocked, in which case C's write
happened-before the unlock and R's snapshot sees it. Either way R can never
silently read a pre-`wv` value while holding a snapshot that should include `wv`.
This is a store-then-load pattern across two locations, which acquire/release
ordering alone does **not** guarantee, so weakening it would need real
justification (and `loom` coverage; see Known gaps).

### The commit protocol (and why the clock is bumped before validation)

1. **Lock** the write-set in ascending id order.
2. **Bump** the global clock: the new value is the write version `wv`.
3. **Validate** each read-set entry: `version <= rv` and not locked by another
   committer. (Skipped if `wv == rv + 1`.)
4. **Write back** every value stamped with `wv`, then **unlock**.

The bump comes after the locks and before validation so that any transaction
that later writes something we validated is guaranteed a larger write version
than ours, i.e. it serialises *after* us. If it had bumped first, it could
serialise before us while we validate against pre-commit state. The same fact
justifies the `wv == rv + 1` shortcut: if the clock moved by exactly our own
bump, no other writer committed in between, so none of our reads can have been
invalidated.

A transaction with an empty write-set skips the whole commit: every read was
already checked against `rv` at the moment it happened, so the reads form a
consistent snapshot. No locks, no clock bump, so read-only transactions never
slow writers down.

### Reads are checked immediately (opacity), not just at commit

`Transaction::read` returns `Err(Conflict)` if the `TVar` is locked by a
committer or its version is newer than `rv`. Validating only at commit would let
a closure run to completion on a mix of old and new values (for example, see one
account debited but not yet the other credited) and could divide by zero or
panic on state that never existed. Aborting at read time means a closure only
ever computes on a consistent snapshot.

### How the closure reports "retry me" vs "my logic failed"

Closures return `Result`, so `?` works naturally.

* `atomically` takes `Fn(&mut Transaction) -> Result<T, Conflict>` and returns
  `T`. The only error is `Conflict`, produced by `tx.read(..)?`, which means
  "retry".
* `try_atomically` takes `Fn(&mut Transaction) -> Result<T, TxError<E>>` and
  returns `Result<T, E>`. `TxError::Conflict` retries; `TxError::Failed(e)`
  discards the buffered writes and returns `Err(e)` without retrying. A
  `From<Conflict> for TxError<E>` impl lets `?` on reads work in both.

A `Failed` result can be trusted because every read that led to it was already
validated against the snapshot.

### Retry strategy

A plain `loop` in `run()`. Each attempt takes a fresh snapshot. A transaction
only retries because another transaction committed (or is committing) a write it
depends on, so the system as a whole always makes progress, but an individual
transaction can starve under sustained contention.

There is **no backoff** yet (deferred checklist item). No livelock was observed
in the tests, including 8 threads hammering 2 accounts (see Evidence), but a
real backoff policy is still needed before claiming robustness under load.

### Type erasure of the read/write sets

A write-set mixes `TVar<i32>`, `TVar<String>`, and so on. The commit protocol
only needs type-independent operations, so `tvar.rs` defines a `pub(crate)`
trait `Cell { id, lock, unlock, validate, store }` implemented for each
`TVar`'s shared state, and the sets hold `Arc<dyn Cell>`. Pending values are
stored as `Box<dyn Any + Send>` and downcast back to `T` inside `store`; the
downcast cannot fail because each write-set entry is created from a typed
`TVar<T>` and `T` together.

Consequences for the public API: `read` needs `T: Clone + Send + 'static`
(values are cloned out of the cell) and `write` needs `T: Send + 'static`.

### Other decisions

* **Initial version of a new `TVar` is 0.** A transaction's `rv` is a clock
  reading, never below 0, so a fresh `TVar` always passes `version <= rv`.
* **`Clock` struct instead of a bare `static`.** The library uses one global
  instance, but unit tests create private ones. The original test asserted exact
  values on the shared static, which would be flaky as soon as another test
  committed a transaction in parallel.
* **`parking_lot::Mutex`** rather than `std::sync::Mutex`: no lock poisoning
  (nothing to unwrap) and slightly faster uncontended.
* **Panics.** A panic in a user closure cannot leave anything locked, because
  locks are only held during `commit`, and `LockedSet` releases them on drop.
* **Lock acquisition spins, then yields.** Committers wait for each other with a
  short spin followed by `yield_now`. The original TL2 paper instead *tries* the
  lock and aborts on failure. Ordered blocking acquisition was chosen to match
  the project checklist; try-lock-and-abort is a possible alternative.

## Deviations from the scaffold

| Scaffold | Implementation | Reason |
|---|---|---|
| `TVar { inner: Arc<Mutex<Inner<T>>> }` | `TVar { shared: Arc<Shared<T>> }` with `id`, `commit_lock`, `cell: Mutex<Inner<T>>` | Needs a lock that outlives a single call and an id for ordering |
| `std::sync::Mutex` | `parking_lot::Mutex` | No poisoning; already a dependency |
| `static GLOBAL_CLOCK: AtomicU64` | `static GLOBAL_CLOCK: Clock` wrapping an `AtomicU64` | Testable in isolation |
| `atomically(F: Fn(&mut Transaction) -> T) -> T` | closure returns `Result<T, Conflict>` | `read` must be able to abort mid-closure without panicking |
| Read-set of `(TVar, recorded version)` | Read-set of `TVar` handles keyed by id | Each read already enforced `version <= rv`; commit re-checks the same bound |
| (not in scaffold) | `try_atomically`, `TxError`, `Conflict` | Answers the "retry vs failure" design question |

## Challenges encountered

* **Stale reads while a commit is in flight.** A reader whose snapshot already
  includes a commit can still see the *old* value if the committer has locked
  but not yet written. Fixed by having readers refuse any locked `TVar`, with the
  `SeqCst` ordering argument above.
* **Heterogeneous write-set.** A single collection of writes to `TVar`s of
  different types, held locked across several steps, doesn't fit a typed
  `MutexGuard`. Solved with the `Cell` trait object plus a flag-based commit
  lock.
* **Deadlock-free locking.** Two committers writing the same pair of `TVar`s in
  opposite orders would deadlock; solved by ordering the write-set by id.
* **Testing a global.** The clock test in the scaffold would be unreliable
  under parallel test execution; solved by splitting out the `Clock` type.
* **Demonstrating the naive failure in safe Rust.** Safe Rust won't compile a
  genuine unsynchronised data race on an `i64`. The naive comparison uses
  atomic loads and stores with a non-atomic read-modify-write, which is a
  *logic* race (lost updates), not undefined behaviour. A `yield_now` between
  the load and the store makes the lost updates show up reliably.

## Evidence

Everything below was measured on a 2-core Linux sandbox; treat the numbers as
indicative, not definitive.

**Correctness tests** (`cargo test`): 20 unit tests, 3 doctests, 3 integration
tests, all passing. The suite was repeated 25 times in release mode and 15 times
in debug mode with no failures.

**Bank-transfer invariant** (`tests/bank_transfer.rs`): 8 threads × 10,000
random transfers. Checked three ways: the final total equals the starting total,
no balance goes negative (overdrafts are rejected with `try_atomically`), and an
auditor thread summing all accounts inside a transaction *while the transfers
run* never sees a total other than the starting one. Run at 8 accounts and at 2
accounts (maximum contention). A typical run: 72,909 transfers completed, 7,091
rejected for insufficient funds, 5,941 audits all passing (8 accounts); 75,957
completed, 4,043 rejected, 38,789 audits passing (2 accounts).

**Naive comparison:** the same transfers with plain load/store and no
transaction: in one run the total drifted from the starting total by +27,804.
The exact drift varies per run; the test requires only that it be non-zero in at
least one of up to five attempts.

**Throughput** (`cargo bench`, short run: 4 threads × 5,000 transfers, 10
samples, 1 s measurement; transfers/second):

| Accounts | STM | Coarse lock | Fine-grained locks |
|---|---|---|---|
| 2 (high contention) | ~0.74 M/s | ~29 M/s | ~17 M/s |
| 8 | ~1.4 M/s | ~28 M/s | ~9.8 M/s |
| 64 (low contention) | ~1.9 M/s | ~30 M/s | ~17 M/s |

Reading these honestly: the STM is roughly 15–40× *slower* than either lock
version on this workload, and the benchmark should say so. Each transfer does two
integer additions, so a mutex costs almost nothing, while every STM transaction
pays for building a read-set and write-set (a `HashMap`, a `BTreeMap`, boxed
values, `Arc` clones) and taking a mutex per read. STM's throughput does improve
as contention drops (2 → 64 accounts), which matches expectations, but it does
not catch the locks here. Fine-grained locks being slower than the single
coarse lock on 2 cores with 4 threads was not investigated. The case for STM is
composability and freedom from lock-ordering bugs, not raw speed on tiny
critical sections. A fair comparison would also need larger transactions and a
machine with more cores; profiling and cheaper set representations (for
example a small inline vector instead of maps for short transactions) are the
obvious next steps.

## Known gaps (update as phases complete)

- No backoff policy yet (Phase 3 checklist item, deferred).
- No `loom` coverage yet (Phase 4). The `loom` dev-dependency is present but
  unused: using it means swapping in `loom`'s atomics and mutex under
  `cfg(loom)`. It is the right tool for checking the memory-ordering argument
  above and for trying weaker orderings.
- Memory ordering is `SeqCst` throughout; not yet relaxed or justified
  piece by piece.
- No blocking `retry` / `or_else` combinators (waiting until a condition on
  some `TVar` becomes true).
- Values are cloned on every read (`T: Clone`); large values would be better
  held behind an `Arc`.
- Per-transaction allocation overhead (see the throughput table).
- Nested `atomically` calls inside a closure run as separate, independent
  transactions; they are not composed into the outer one.
- No distributed extension yet (see `ROADMAP.md`, written in Phase 7).

## References

- Dice, Shalev, Shavit. *Transactional Locking II.* DISC 2006. (The TL2
  algorithm implemented here.)
