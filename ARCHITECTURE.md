# Architecture

Design notes for `stm`, an implementation of the TL2 algorithm (Dice, Shalev and Shavit, *Transactional Locking II*, DISC 2006). This file records what was built, why each decision was made, how correctness was argued and tested, and what the measurements showed.

## Overview

```
atomically(|tx| { ... })
   │
   ├─ run loop (transaction.rs)          retry until commit, with exponential backoff
   │    ├─ Transaction::new()            rv = clock::read_clock()
   │    ├─ body(&mut tx)                 tx.read (validated immediately) / tx.write (buffered)
   │    └─ tx.commit()                   lock → bump clock → validate → write back → unlock
   │
   ├─ TVar<T>   (tvar.rs)                value + version under a mutex, plus a commit-lock flag
   ├─ Clock     (clock.rs)               one global AtomicU64
   ├─ stats     (stats.rs)               thread-local outcome counters
   └─ sync      (sync.rs)                std / parking_lot, or loom under --cfg loom
```

## The commit protocol

1. **Lock** the write-set in ascending `TVar` id order.
2. **Bump** the global clock. The new value is the write version `wv`.
3. **Validate** each read-set entry: its version is still `<= rv` and no other committer holds its lock. Skipped when `wv == rv + 1`.
4. **Write back** every buffered value stamped with `wv`, then **unlock**.

**Why bump after locking and before validating.** Locking first means any transaction that later learns `wv` from the clock also sees our locks (see *Memory ordering*), so it cannot read a value we are about to overwrite. Bumping before validating means any transaction that commits a write to something we read, after our validation, must have bumped the clock after us, so it gets a larger write version and serializes after us. The same fact justifies the shortcut in step 3: if the clock moved only by our own bump, no other writer committed in between.

`tests/loom.rs` checks this ordering directly. Moving the bump before the lock (mutation M4 below) fails every loom test.

**Read-only transactions** skip the commit entirely. Each read was already checked against `rv` when it happened, so together the reads form a consistent snapshot. No locks and no clock bump, so readers never slow writers down.

## Reads are validated immediately (opacity)

`Transaction::read` returns `Err(Conflict)` if the `TVar` is locked by a committer or its version is newer than `rv`. Validating only at commit would let a body run on a mix of old and new values (one account debited, the other not yet credited) and possibly divide by zero or loop forever on a state that never existed. Aborting at read time means a body only ever computes on a consistent snapshot, which is the property Guerraoui and Kapalka call *opacity*.

## `TVar<T>` internals

**One mutex holds the value and the version together.** The alternative, a separate `AtomicU64` version beside a `Mutex<T>`, lets a reader read version 5, have a writer install a value at version 9, then read that value, and believe it holds "the value as of version 5". Putting both under one mutex makes the pair impossible to tear. The mutex is held only for a clone or a swap.

**A separate `commit_lock: AtomicBool`.** A committer must hold locks on `TVar`s of several different `T` across several protocol steps. A `MutexGuard<'_, Inner<T>>` is typed by `T`, so it cannot live in one heterogeneous collection. A flag can be set and cleared through a type-erased interface. Readers never block on it; they abort and retry.

**Type erasure.** A `pub(crate)` trait `Cell { lock, unlock, validate, store }` is implemented for every `TVar`'s shared state. The transaction's sets hold `&dyn Cell`, and pending values are `Box<dyn Any + Send>`, downcast back to `T` in `store`. The downcast cannot fail, because each entry is created from a typed `TVar<T>` together with a value of type `T`.

**Ids for lock ordering.** Each `TVar` gets a unique `u64` id from a global counter when it is created; clones share it. Committers lock in ascending id order, so no cycle of "A waits for B waits for A" can form. An id was chosen over the heap address so the order is explicit.

**Initial version is 0**, which is never above any read version, so a fresh `TVar` is readable by every transaction.

## Transaction representation

**Borrowed references, not reference counts.** `Transaction<'env>` holds `&'env dyn Cell` references to the `TVar`s it touched, and `read`/`write` take `&'env TVar<T>`. The first version cloned an `Arc` per read and per write. Every clone and drop is an atomic read-modify-write on a cache line that all threads share, so this was a real source of contention. With references, recording a read costs no atomic operation, and the borrow checker guarantees every `TVar` outlives the transactions that point at it. A body that creates a `TVar` and reads it is now a compile error (there is a `compile_fail` doctest for this). For ordinary closures the API is unchanged. A helper function that takes a transaction must name the shared lifetime: `fn f<'env>(tx: &mut Transaction<'env>, v: &'env TVar<i64>)`.

**Inline small vectors, not maps.** The read-set is a `SmallVec` of `(id, &dyn Cell)` with 8 inline slots. The write-set is a `SmallVec` of `(id, &dyn Cell, boxed value)` with 4 inline slots, with at most one entry per `TVar`. It is sorted by id once, at commit, which gives the lock order; `binary_search` then answers "is this read also one of my writes?" during validation. The first version used a `HashMap` and a `BTreeMap`, which cost two heap allocations per transaction. Typical transactions are small, so a linear scan for read-your-own-writes beats hashing. A transaction that writes thousands of `TVar`s would want a map; that is a known trade-off.

**Duplicate reads.** Reading the same `TVar` twice adds two read-set entries. That costs one redundant validation and saves a lookup on every read.

**`LockedSet`** owns the sorted write-set while it is locked and unlocks every locked entry when dropped, so an early `return false` from validation, or a panic, can never leave a `TVar` locked.

## Memory ordering

The clock is read with `Acquire` and bumped with an `AcqRel` `fetch_add`. The commit-lock flag is taken with an `AcqRel` compare-exchange, released with a `Release` store, and checked by readers and validators with `Acquire` loads.

**The argument.** Take a committer C that will write at version `wv`, and a reader R whose snapshot `rv >= wv` includes C's commit.

1. C sets its lock flags (compare-exchange), *then* bumps the clock (release).
2. R reads the clock (acquire) and gets `wv` or a later value. If later, the value was written by some other committer's `fetch_add`. Every bump is a read-modify-write, so it continues C's *release sequence*, and R still synchronizes with C.
3. So C's lock acquisitions *happen before* R's flag check. R sees the flag set (and aborts) or sees a later state, in which case C has written back, and R reads C's value and version through the cell mutex.

So R can never read a pre-`wv` value while holding a snapshot that includes `wv`. Validation is covered by the same reasoning: two committers' clock bumps are totally ordered, and the later one acquires the earlier one's locks.

**A correction.** The first version used `SeqCst` everywhere, and this document claimed acquire/release was *insufficient* because the pattern was "a store followed by a load on a different location". That was wrong. It is a message-passing pattern carried by the clock: the reader learns about the commit *through* the clock value it acquires. Loom (which models acquire/release precisely and treats `SeqCst` the same way) passes every scenario with the weaker orderings, and fails them all if the clock is made `Relaxed` (mutation M3), which shows the clock's ordering is the one carrying the argument.

**The unlock store** is `Release`, as any lock's should be. Loom also passes with a `Relaxed` unlock (mutation M5). That is expected: readers read the value and version under the cell mutex, and the mutex orders them after the committer's write-back. `Release` is kept so the flag is a correct lock on its own rather than depending on that.

## Retry and backoff

`run()` loops until a commit succeeds. Between attempts it backs off: after the k-th consecutive failure it spins for about `2^k` pause instructions (k capped at 6), then yields the thread.

**Measured effect.** Backoff made no difference to the original implementation, but once transactions became cheap it became essential. Without it, 100 threads on 2 accounts collapse to 0.04 M commits/s with 99.98% of attempts retried, which is effectively livelock. With it, the same run sustains 1.6 M/s with about 43% retried. The original code's heap allocations had been acting as accidental backoff. In the criterion benchmark, removing backoff from the final version cuts throughput from 2.81 to 0.74 M/s at 2 accounts.

**Progress.** A transaction only retries because another transaction committed or is committing a write it depends on, so the system as a whole always makes progress. An individual transaction can still starve under sustained contention; there is no fairness mechanism.

## Instrumentation

`stats.rs` keeps four counters per thread: commits, read conflicts, validation failures, and user aborts. They are thread-local on purpose. A single global counter would be one more cache line that every committing thread writes. The live dashboard reads each worker's `thread_stats()` and publishes it to a cache-line-aligned slot.

With `--features trace`, every attempt is logged to stderr with its thread, attempt number, read version, set sizes, and outcome. The feature compiles to nothing when it is off.

## Errors versus retry

Closures return `Result`, so `?` works.

- `atomically` takes `Fn(&mut Transaction) -> Result<T, Conflict>` and returns `T`. The only error is `Conflict`, produced by `tx.read(..)?`, which means "retry".
- `try_atomically` takes `Fn(&mut Transaction) -> Result<T, TxError<E>>` and returns `Result<T, E>`. `TxError::Conflict` retries; `TxError::Failed(e)` discards the buffered writes and returns `Err(e)` without retrying. A `From<Conflict>` impl lets `?` on reads work in both.

A `Failed` result can be trusted because every read that led to it was validated against the snapshot.

## How the tests were tested

**The loom setup.** `src/sync.rs` re-exports `std` atomics and `parking_lot::Mutex` normally, and `loom`'s instrumented versions under `--cfg loom`. Spin and yield hints become `loom::thread::yield_now()` so loom can schedule around busy-waits. The global clock becomes a `loom::lazy_static!` so every execution starts fresh. The loom tests therefore exercise the real library code, not a copy of it.

**Scenarios** (two threads each): opposite-direction transfers (deadlock and lost updates), a reader concurrent with a transfer (partial-state reads), concurrent increments (lost updates), and write skew (serializability). At preemption bound 4 they explore 518, 3,029, 624 and 1,135 interleavings.

**Mutation testing.** To confirm the suite can fail, bugs were planted one at a time:

| Mutation | Loom result |
|---|---|
| M1: skip read-set validation | 3 of 4 tests fail |
| M2: readers ignore the commit lock | 4 of 4 fail |
| M3: clock operations `Relaxed` | 4 of 4 fail |
| M4: bump the clock before locking | 4 of 4 fail |
| M5: commit-lock release `Relaxed` | all pass (expected, see *Memory ordering*) |

**A trap found along the way.** Loom's unbounded mode looked like the stronger option and finished faster, but it explored only 22 executions of the reader scenario and missed M2, which bounds 2, 3 and 4 all catch. The suite therefore uses a bound of 4 by default (override with `STM_LOOM_PREEMPTIONS`).

**The naive comparison** in `tests/bank_transfer.rs` performs the same transfers with atomic loads and stores but a non-atomic read-modify-write. Safe Rust will not compile a true data race on an `i64`, so this is a *logic* race (lost updates), not undefined behaviour. A `yield_now` between the load and the store makes it fail reliably.

## Evidence

All measurements are from a 2-core Linux machine; treat them as relative.

**Tests:** 35 from `cargo test` (27 unit, 3 integration, 5 doc, including one `compile_fail`) and 4 loom scenarios, all passing.

**Throughput** (criterion, 4 threads x 5,000 transfers per iteration, 20 samples, 5 s measurement; million transfers per second):

| Accounts | STM | Coarse lock | Per-account locks |
|---|---|---|---|
| 2 (high contention) | 2.81 | 30.29 | 18.52 |
| 8 | 2.80 | 32.49 | 9.16 |
| 64 (low contention) | 4.68 | 27.52 | 15.48 |

**Optimization ablation** (same settings, STM only):

| Version | 2 | 8 | 64 |
|---|---|---|---|
| Original | 0.38 | 1.05 | 2.10 |
| + backoff | 0.45 | 1.02 | 2.01 |
| + inline sets | 1.46 | 2.08 | 3.56 |
| + borrowed references (final) | 2.81 | 2.80 | 4.68 |
| final without backoff | 0.74 | 1.02 | 2.69 |

**Reading these honestly.** STM is 6 to 12 times slower than one coarse lock and 3 to 7 times slower than per-account locks on this workload. Each transfer is two integer additions, so a mutex costs almost nothing, while every transaction still pays for a snapshot, a boxed value per write, a mutex acquisition per read, a contended clock bump per commit, and validation. The case for STM is composability and the absence of lock-ordering bugs. In `examples/deadlock_demo.rs`, where the critical section yields while holding its locks, STM finished in 300 to 400 ms against 600 to 650 ms for correctly ordered locks in 5 of 5 runs (timings have 50 ms resolution), because an STM transaction holds no lock while it waits. Per-account locks being slower than one coarse lock at every contention level was not investigated in depth; with only 2 cores, there is little parallelism for fine-grained locking to exploit.

**Live demo:** 100 threads on 2 accounts for 6 s gave 9.4 M commits, 43.7% of attempts retried, and 1,014,630 concurrent audits with 0 violations.

## Challenges encountered

- **Stale reads during an in-flight commit.** A reader whose snapshot includes a commit could see the old value if the committer had locked but not yet written. Fixed by having readers refuse any locked `TVar`, justified by the ordering argument above and checked by loom (M2).
- **A heterogeneous write-set held across several steps.** It does not fit a typed `MutexGuard`. Solved with the `Cell` trait object and a flag-based commit lock.
- **Deadlock-free commits.** Solved by locking in ascending id order, demonstrated by the deadlock demo and the opposite-transfer loom test.
- **Getting the memory-ordering argument right.** The first written argument was wrong (see *A correction*). Model checking plus mutation testing turned it from an assertion into something checked.
- **Livelock after optimizing.** Making transactions cheaper exposed a near-livelock that the slower code had hidden. Fixed with exponential backoff.
- **Trusting the test tool.** Loom's unbounded mode silently explored less. Planting known bugs is what revealed it.
- **Testing a global.** The scaffold's clock test asserted exact values on the shared static, which is flaky under parallel tests. Fixed by splitting out a `Clock` type that tests can create privately.

## Known limitations

- **No blocking `retry` / `or_else`** (waiting until some `TVar` changes, as in Haskell's STM).
- **Nested `atomically` calls** inside a body run as separate transactions; they are not flattened into the outer one.
- **Values are cloned on every read** (`T: Clone`). Large values should be stored behind an `Arc`.
- **Each write boxes its value**, which is one heap allocation per write.
- **The global clock is one contended cache line.** TL2's GV4/GV5 clock variants reduce this.
- **No fairness or contention manager**, so an individual transaction can starve.
- **Loom coverage is bounded** (two threads, preemption bound 4). It is systematic, not a proof for all schedules.
- **No distributed extension yet.** See `ROADMAP.md`.

## Deviations from the original scaffold

| Scaffold | Implementation | Reason |
|---|---|---|
| `TVar { inner: Arc<Mutex<Inner<T>>> }` | `TVar { shared: Arc<Shared<T>> }` with `id`, `commit_lock`, `cell` | A lock that outlives one call, and an id for ordering |
| `std::sync::Mutex` | `parking_lot::Mutex` (loom's under `--cfg loom`) | No poisoning; swappable for model checking |
| `static GLOBAL_CLOCK: AtomicU64` | `Clock` type wrapping an `AtomicU64` | Testable in isolation |
| `atomically(F: Fn(&mut Transaction) -> T) -> T` | Body returns `Result<T, Conflict>` | Reads must be able to abort mid-body |
| Read-set of `(TVar, version)` | `(id, &dyn Cell)`, no stored version | Each read already enforced `version <= rv` |
| `SeqCst` (first implementation) | Acquire/release | Sufficient by the argument above; checked by loom |
| (not in scaffold) | `try_atomically`, `TxError`, `Conflict`, `stats`, `trace` | Retry-versus-failure, instrumentation |
