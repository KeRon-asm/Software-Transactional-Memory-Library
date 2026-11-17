# Roadmap

Where `stm` could go next, roughly in order of value. The first group improves the single-machine library; the second is the larger plan of extending transactions across machines.

## Single-machine improvements

1. **Blocking `retry` and `or_else`.** Let a transaction say "wait until something I read changes" (for example, block until a queue is non-empty) instead of spinning. This needs a per-`TVar` wait list that committers wake. It is the feature that makes STM compose for producer/consumer code, as in Harris et al., *Composable Memory Transactions*.
2. **Flattened nesting.** Make an `atomically` call inside a transaction join the outer transaction instead of starting an independent one, so library functions built on `atomically` compose into larger atomic operations.
3. **Cheaper writes.** Remove the one heap allocation per write (`Box<dyn Any>`) with a per-thread arena or an inline buffer for small `Copy` values.
4. **A less contended clock.** Every writing commit bumps one global counter. TL2's GV4 ("pass on failure") and GV5/GV6 variants reduce how often that cache line moves between cores.
5. **Lock-free reads.** Replace the per-read mutex with a sequence lock (version, value, version again), which needs careful `unsafe` code and new loom tests.
6. **Contention management.** Track per-transaction retries and give long-starved transactions priority, so an individual transaction cannot starve indefinitely.
7. **Wider verification.** Three-thread loom scenarios, a `cargo-fuzz` harness generating random transaction mixes checked against a sequential model, and benchmarks on a many-core machine where fine-grained approaches have room to scale.

## Distributed STM

The goal is to keep the same `atomically` API while `TVar`s live on different machines.

### Phase 1: two-phase commit

- Each node owns a shard of `TVar`s, identified by `(node, id)`.
- Reads go to the owning node and return `(value, version)`. The transaction keeps its read-set and write-set locally, as now.
- Commit becomes two-phase commit with the transaction's node as coordinator:
  1. **Prepare:** send each participant its part of the write-set and read-set. Each participant locks its write entries in id order, validates its read entries, and votes yes or no.
  2. **Commit or abort:** if every vote is yes, send commit with the write version; participants apply and unlock. Otherwise, send abort.
- The global clock is replaced by hybrid logical clocks (or a timestamp oracle), because one shared counter does not scale across machines.
- **Known weakness:** two-phase commit blocks if the coordinator fails after prepare, leaving participants holding locks.

### Phase 2: fault tolerance with Raft

- Replicate each shard with Raft, so a shard survives the loss of a minority of its replicas.
- Log the coordinator's decision in a Raft group too, so a new coordinator can finish or abort an in-flight transaction after a crash. This removes 2PC's blocking failure.
- This is essentially the design used by systems such as Spanner and CockroachDB (transactions over consensus-replicated shards), which is a useful reference point for the trade-offs.

### Open questions

- How to bound retry cost when a conflict is detected only after network round-trips.
- Whether optimistic validation (as here) or pessimistic locking performs better when cross-node latency dominates.
- How to test it: deterministic simulation of the network and node failures (in the style of FoundationDB or `madsim`) would play the role loom plays for the single-machine library.
