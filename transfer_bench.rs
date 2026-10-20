//! Criterion benchmarks: throughput of the STM-based bank-transfer demo
//! vs. the lock-based baselines (Phase 6 checklist item).
//!
//! Three implementations run the SAME transfer workload:
//!
//! - `stm`: each transfer is one `atomically` transaction.
//! - `coarse_lock`: one mutex around every account (simple, but all
//!   transfers are serialised).
//! - `fine_locks`: one mutex per account, taken in ascending index order to
//!   avoid deadlock (more parallelism, but the programmer has to get the
//!   lock ordering right).
//!
//! Contention is varied by changing how many accounts the threads share:
//! few accounts means nearly every pair of transfers collides, many accounts
//! means they rarely do. Each benchmark reports throughput in transfers per
//! second, so the groups can be compared directly.
//!
//! Run with `cargo bench`. Absolute numbers depend heavily on the machine
//! (in particular the number of cores), so compare the approaches against
//! each other on the same run rather than across machines.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use parking_lot::Mutex;
use std::hint::black_box;
use std::thread;
use stm::{TVar, atomically};

const THREADS: usize = 4;
const TRANSFERS_PER_THREAD: usize = 5_000;
const STARTING_BALANCE: i64 = 1_000;
/// Account counts to benchmark: high, medium and low contention.
const ACCOUNT_COUNTS: [usize; 3] = [2, 8, 64];

/// One implementation under test: runs the whole workload over the given
/// number of accounts and returns the final total balance.
type Approach = fn(usize) -> i64;

/// A tiny deterministic xorshift generator. Every approach consumes exactly
/// the same random sequence per thread, so they do identical work, and the
/// RNG itself costs next to nothing compared with a transfer.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        // Mix the seed and make sure the state is never zero.
        XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Two different account indices below `n` (`n >= 2`) and an amount.
    fn transfer(&mut self, n: usize) -> (usize, usize, i64) {
        let from = (self.next() % n as u64) as usize;
        let mut to = (self.next() % (n as u64 - 1)) as usize;
        if to >= from {
            to += 1;
        }
        let amount = (self.next() % 100) as i64 + 1;
        (from, to, amount)
    }
}

/// STM: every transfer is a transaction over two `TVar`s.
fn run_stm(account_count: usize) -> i64 {
    let accounts: Vec<TVar<i64>> = (0..account_count)
        .map(|_| TVar::new(STARTING_BALANCE))
        .collect();

    thread::scope(|scope| {
        for t in 0..THREADS {
            let accounts = &accounts;
            scope.spawn(move || {
                let mut rng = XorShift::new(t as u64 + 1);
                for _ in 0..TRANSFERS_PER_THREAD {
                    let (from, to, amount) = rng.transfer(account_count);
                    atomically(|tx| {
                        let a = tx.read(&accounts[from])?;
                        let b = tx.read(&accounts[to])?;
                        tx.write(&accounts[from], a - amount);
                        tx.write(&accounts[to], b + amount);
                        Ok(())
                    });
                }
            });
        }
    });

    atomically(|tx| {
        let mut sum = 0;
        for account in &accounts {
            sum += tx.read(account)?;
        }
        Ok(sum)
    })
}

/// Coarse-grained locking: one mutex guards all the accounts.
fn run_coarse_lock(account_count: usize) -> i64 {
    let accounts = Mutex::new(vec![STARTING_BALANCE; account_count]);

    thread::scope(|scope| {
        for t in 0..THREADS {
            let accounts = &accounts;
            scope.spawn(move || {
                let mut rng = XorShift::new(t as u64 + 1);
                for _ in 0..TRANSFERS_PER_THREAD {
                    let (from, to, amount) = rng.transfer(account_count);
                    let mut guard = accounts.lock();
                    guard[from] -= amount;
                    guard[to] += amount;
                }
            });
        }
    });

    accounts.lock().iter().sum()
}

/// Fine-grained locking: one mutex per account, always locking the lower
/// index first so two transfers can never wait on each other in a cycle.
fn run_fine_locks(account_count: usize) -> i64 {
    let accounts: Vec<Mutex<i64>> = (0..account_count)
        .map(|_| Mutex::new(STARTING_BALANCE))
        .collect();

    thread::scope(|scope| {
        for t in 0..THREADS {
            let accounts = &accounts;
            scope.spawn(move || {
                let mut rng = XorShift::new(t as u64 + 1);
                for _ in 0..TRANSFERS_PER_THREAD {
                    let (from, to, amount) = rng.transfer(account_count);
                    let (first, second) = if from < to { (from, to) } else { (to, from) };
                    let mut first_guard = accounts[first].lock();
                    let mut second_guard = accounts[second].lock();
                    if from < to {
                        *first_guard -= amount;
                        *second_guard += amount;
                    } else {
                        *second_guard -= amount;
                        *first_guard += amount;
                    }
                }
            });
        }
    });

    accounts.iter().map(|account| *account.lock()).sum()
}

fn bench_transfers(c: &mut Criterion) {
    let mut group = c.benchmark_group("bank_transfers");
    group.sample_size(20);
    group.throughput(Throughput::Elements(
        (THREADS * TRANSFERS_PER_THREAD) as u64,
    ));

    let approaches: [(&str, Approach); 3] = [
        ("stm", run_stm),
        ("coarse_lock", run_coarse_lock),
        ("fine_locks", run_fine_locks),
    ];

    for account_count in ACCOUNT_COUNTS {
        let expected_total = STARTING_BALANCE * account_count as i64;
        for (name, run) in approaches {
            group.bench_with_input(
                BenchmarkId::new(name, format!("{account_count}_accounts")),
                &account_count,
                |b, &account_count| {
                    b.iter(|| {
                        let total = run(black_box(account_count));
                        // Keep every approach honest: money is conserved.
                        assert_eq!(total, expected_total);
                    });
                },
            );
        }
    }

    group.finish();
}

criterion_group!(benches, bench_transfers);
criterion_main!(benches);
