//! Multi-threaded integration test: concurrent bank transfers.
//!
//! Several threads each make many random transfers between shared accounts
//! via `atomically`, and we assert the one invariant that must never break
//! no matter how the threads interleave: the TOTAL balance across all
//! accounts is unchanged.
//!
//! Three checks back that up for the STM version:
//!
//!   1. After all threads join, the accounts still sum to the starting total
//!      and none went negative (transfers that would overdraw are rejected
//!      with `try_atomically`).
//!   2. While the transfers are running, an auditor thread repeatedly sums
//!      every account inside its own transaction. Every sum it observes must
//!      equal the starting total — i.e. no transaction ever sees "money in
//!      flight" (one account debited, the other not yet credited).
//!   3. The same workload runs at two contention levels: 8 accounts, and just
//!      2 accounts so every transfer collides with every other.
//!
//! A SECOND test runs the same transfers WITHOUT the STM (plain load, then
//! store, with no locking) and shows the invariant breaking. That contrast is
//! the evidence that the STM layer is doing real work.
//!
//! Run with `cargo test --test bank_transfer -- --nocapture` to see the
//! counts printed by each test.

use rand::random_range;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use stm::{TVar, TxError, atomically, try_atomically};

const STARTING_BALANCE: i64 = 500;
const MAX_TRANSFER: i64 = 100;

/// Pick two DIFFERENT account indices out of `n` (`n >= 2`).
///
/// Transferring from an account to itself is meaningless (it would read the
/// same balance twice and "create" money), so callers never ask for it.
fn pick_pair(n: usize) -> (usize, usize) {
    let from = random_range(0..n);
    let mut to = random_range(0..n - 1);
    if to >= from {
        to += 1;
    }
    (from, to)
}

#[derive(Debug, PartialEq, Eq)]
struct InsufficientFunds;

/// Move `amount` from `from` to `to`, or fail (changing nothing) if `from`
/// can't cover it.
fn transfer(from: &TVar<i64>, to: &TVar<i64>, amount: i64) -> Result<(), InsufficientFunds> {
    try_atomically(|tx| {
        let from_balance = tx.read(from)?;
        if from_balance < amount {
            return Err(TxError::Failed(InsufficientFunds));
        }
        let to_balance = tx.read(to)?;
        tx.write(from, from_balance - amount);
        tx.write(to, to_balance + amount);
        Ok(())
    })
}

/// Sum all accounts inside ONE transaction, so the result is a consistent
/// snapshot of a single point in time.
fn audit(accounts: &[TVar<i64>]) -> i64 {
    atomically(|tx| {
        let mut sum = 0;
        for account in accounts {
            sum += tx.read(account)?;
        }
        Ok(sum)
    })
}

struct Outcome {
    completed: usize,
    rejected: usize,
    audits: usize,
}

/// Run `threads` workers, each attempting `transfers_per_thread` random
/// transfers between `account_count` accounts, while an auditor thread
/// checks the total the whole time. Panics (failing the test) on any
/// violated invariant.
fn run_stm_transfers(account_count: usize, threads: usize, transfers_per_thread: usize) -> Outcome {
    let accounts: Vec<TVar<i64>> = (0..account_count)
        .map(|_| TVar::new(STARTING_BALANCE))
        .collect();
    let expected_total = STARTING_BALANCE * account_count as i64;
    let done = AtomicBool::new(false);

    let (completed, rejected, audits) = thread::scope(|scope| {
        let auditor = scope.spawn(|| {
            let mut audits = 0;
            while !done.load(Ordering::SeqCst) {
                assert_eq!(
                    audit(&accounts),
                    expected_total,
                    "auditor observed a total that is not the starting total"
                );
                audits += 1;
            }
            audits
        });

        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let (mut completed, mut rejected) = (0, 0);
                    for _ in 0..transfers_per_thread {
                        let (from, to) = pick_pair(account_count);
                        let amount = random_range(1..=MAX_TRANSFER);
                        match transfer(&accounts[from], &accounts[to], amount) {
                            Ok(()) => completed += 1,
                            Err(InsufficientFunds) => rejected += 1,
                        }
                    }
                    (completed, rejected)
                })
            })
            .collect();

        let (mut completed, mut rejected) = (0, 0);
        for worker in workers {
            let (c, r) = worker.join().unwrap();
            completed += c;
            rejected += r;
        }
        done.store(true, Ordering::SeqCst);
        (completed, rejected, auditor.join().unwrap())
    });

    // Final check, after every thread has finished.
    assert_eq!(audit(&accounts), expected_total, "total balance changed");
    for (index, account) in accounts.iter().enumerate() {
        let balance = atomically(|tx| tx.read(account));
        assert!(balance >= 0, "account {index} went negative: {balance}");
    }
    assert_eq!(completed + rejected, threads * transfers_per_thread);

    Outcome {
        completed,
        rejected,
        audits,
    }
}

#[test]
fn concurrent_transfers_preserve_total_balance() {
    let outcome = run_stm_transfers(8, 8, 10_000);
    eprintln!(
        "8 accounts: {} transfers completed, {} rejected (insufficient funds), {} audits passed",
        outcome.completed, outcome.rejected, outcome.audits
    );
    assert!(outcome.completed > 0);
}

#[test]
fn high_contention_transfers_preserve_total_balance() {
    // Two accounts, many threads: every transaction conflicts with nearly
    // every other, so this exercises the retry path hard (and would hang if
    // the commit protocol could deadlock or livelock outright).
    let outcome = run_stm_transfers(2, 8, 10_000);
    eprintln!(
        "2 accounts: {} transfers completed, {} rejected (insufficient funds), {} audits passed",
        outcome.completed, outcome.rejected, outcome.audits
    );
    assert!(outcome.completed > 0);
}

/// The same transfers with NO transactions: each step is an atomic load or
/// store on its own, but the read-modify-write as a whole is not atomic, so
/// concurrent updates get lost and money is created or destroyed.
///
/// (Safe Rust won't let us write a genuine unsynchronised data race on a
/// shared `i64`, so this uses atomics — which makes it a *logic* race, not
/// undefined behaviour. `yield_now` between the load and the store widens the
/// window so the lost updates show up reliably, even on one core.)
fn run_naive_transfers(account_count: usize, threads: usize, transfers_per_thread: usize) -> i64 {
    let accounts: Vec<AtomicI64> = (0..account_count)
        .map(|_| AtomicI64::new(STARTING_BALANCE))
        .collect();
    let expected_total = STARTING_BALANCE * account_count as i64;

    thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                for _ in 0..transfers_per_thread {
                    let (from, to) = pick_pair(account_count);
                    let amount = random_range(1..=MAX_TRANSFER);

                    // Non-atomic read-modify-write on each account.
                    let from_balance = accounts[from].load(Ordering::SeqCst);
                    thread::yield_now();
                    accounts[from].store(from_balance - amount, Ordering::SeqCst);

                    let to_balance = accounts[to].load(Ordering::SeqCst);
                    thread::yield_now();
                    accounts[to].store(to_balance + amount, Ordering::SeqCst);
                }
            });
        }
    });

    let actual_total: i64 = accounts.iter().map(|a| a.load(Ordering::SeqCst)).sum();
    actual_total - expected_total
}

#[test]
fn naive_unsynchronized_transfers_break_total_balance() {
    // A single run could, in principle, get lucky; try a few times and
    // require that the invariant breaks at least once.
    const ATTEMPTS: usize = 5;

    let mut drifts = Vec::new();
    for _ in 0..ATTEMPTS {
        let drift = run_naive_transfers(8, 8, 2_000);
        drifts.push(drift);
        if drift != 0 {
            break;
        }
    }

    eprintln!("naive version: total drifted from the starting total by {drifts:?}");
    assert!(
        drifts.iter().any(|&drift| drift != 0),
        "expected the unsynchronised version to lose or create money, but the \
         total was preserved in all {ATTEMPTS} attempts"
    );
}
