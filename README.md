# Software Transactional Memory Library (Rust)

A Software Transactional Memory (STM) library written in Rust, built on the **TL2 (Transactional Locking II)** algorithm. It lets multiple threads read and write shared state inside atomic transactions without the programmer managing locks by hand.

> **Status: work in progress.** This is a course project for CSC 4330. The API below is the design target; items are checked off in the roadmap as they are completed.

## Why STM?

Lock-based concurrency is error-prone: forget a lock and you get a data race, take locks in the wrong order and you get a deadlock. STM replaces manual locking with **optimistic concurrency control**:

1. A transaction runs against a snapshot of shared memory, recording what it reads and buffering what it writes.
2. At commit time, the library checks that nothing it read has changed.
3. If validation passes, all writes are applied atomically. If not, the transaction is discarded and retried.

The programmer writes the transaction body; the library guarantees atomicity and isolation.

## Core ideas (TL2)

- **`TVar<T>`**: a transactional variable holding a value and a version number.
- **Global version clock**: an atomic counter incremented on every successful write-commit.
- **Read version**: each transaction snapshots the global clock when it starts.
- **Read-set / write-set**: reads record the version seen; writes are buffered and not visible until commit.
- **Commit protocol**: lock the write-set in a fixed global order (avoids deadlock), validate the read-set against the read version, apply writes, bump the clock, unlock.
- **Abort and retry**: on validation failure, discard the sets and re-run the transaction body.

## Planned usage

```rust
// Target API (not implemented yet)
let from = TVar::new(100);
let to = TVar::new(50);

atomically(|tx| {
    let a = tx.read(&from)?;
    let b = tx.read(&to)?;
    tx.write(&from, a - 10)?;
    tx.write(&to, b + 10)?;
    Ok(())
});
```

## Project layout

```
src/
  lib.rs
  tvar.rs          # TVar<T>: value + version
  transaction.rs   # read-set, write-set, commit protocol, atomically()
  clock.rs         # global version clock
tests/             # integration tests (concurrent bank transfers)
benches/           # criterion benchmarks
```

## Building and testing

```bash
cargo build
cargo test
cargo clippy
cargo fmt --check
```

## Roadmap

- [ ] `TVar<T>` and global version clock
- [ ] Transaction context (read-set, write-set, read version)
- [ ] `atomically` with commit and abort/retry
- [ ] Single-threaded unit tests
- [ ] Multi-threaded bank-transfer test (total balance invariant)
- [ ] `loom` tests for the commit protocol
- [ ] Baselines: coarse-grained lock and per-account locks
- [ ] Benchmarks across contention levels (`criterion`)
- [ ] Live commit/abort visualization
- [ ] Future: distributed STM (two-phase commit, then Raft-based fault tolerance)

## References

1. N. Shavit and D. Touitou, "Software Transactional Memory," 1995.
2. D. Dice, O. Shalev, and N. Shavit, "Transactional Locking II," 2006.
3. T. Harris, S. Marlow, S. Peyton Jones, and M. Herlihy, "Composable Memory Transactions," 2005.
4. M. Herlihy and N. Shavit, *The Art of Multiprocessor Programming*, Ch. 18.

## AI tool disclosure

AI assistance (Claude) was used for mentoring, explanations, and drafting documentation for this project, per the course acknowledgment requirement.

## License

MIT
