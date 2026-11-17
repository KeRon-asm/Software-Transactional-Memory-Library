# Changelog

All notable changes to this project. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [semantic versioning](https://semver.org/).

## [1.0.0] - 2026-11

### Added
- `loom` model-checking suite (`tests/loom.rs`): four two-thread scenarios (opposite transfers, reader during a transfer, concurrent increments, write skew), about 5,300 interleavings at preemption bound 4, run against the real code through a new `sync` module.
- Per-thread transaction statistics (`stm::thread_stats()`, `TxStats`): commits, read conflicts, validation failures, user aborts.
- `trace` cargo feature that logs every transaction attempt to stderr.
- Bounded exponential backoff between retries.
- `examples/bank_demo.rs`: live terminal dashboard (throughput, conflict rate, per-thread grid, continuous invariant audit) for 50 to 100+ threads.
- `examples/deadlock_demo.rs`: per-account locks taken in transfer order deadlock; ordered locks and STM complete.
- `examples/conflict_trace.rs`: a single two-thread conflict and retry, narrated.
- `compile_fail` doctest showing a dangling `TVar` reference is rejected.
- GitHub Actions CI: build, test, clippy, rustfmt, loom.
- `ROADMAP.md` (distributed STM plan), benchmark charts and dashboard screenshots in `docs/images/`.

### Changed
- **Performance: 2.2 to 7.4 times faster** than 0.1.0 depending on contention (criterion, 2 cores).
  - Read-set and write-set are inline `SmallVec`s instead of a `HashMap` and a `BTreeMap`.
  - `Transaction<'env>` holds borrowed `&'env` references instead of cloning an `Arc` per read and write. Helper functions taking a `Transaction` now name its lifetime.
- Memory ordering relaxed from `SeqCst` to acquire/release, with a corrected argument in `ARCHITECTURE.md`, checked by loom and by mutation testing.
- `TVar` handles in the commit path are `&dyn Cell` instead of `Arc<dyn Cell>`.

### Removed
- Unused `crossbeam` dependency.

## [0.1.0] - 2026-10-16

### Added
- TL2 core: global version clock, `TVar<T>` (value and version under one mutex, plus a commit-lock flag), `atomically`, `try_atomically`, `TxError`, `Conflict`.
- Commit protocol: lock the write-set in ascending id order, bump the clock, validate the read-set, write back, unlock; read-only fast path; `wv == rv + 1` validation shortcut.
- Reads validated immediately (opacity).
- Unit tests, multi-threaded bank-transfer tests with a concurrent auditor, and a naive unsynchronized comparison that loses money.
- Criterion benchmarks against coarse-grained and per-account locking.
- `ARCHITECTURE.md` design notes.

[1.0.0]: https://github.com/KeRon-asm/Software-Transactional-Memory-Library/compare/v0.1...v1.0
[0.1.0]: https://github.com/KeRon-asm/Software-Transactional-Memory-Library/releases/tag/v0.1
