//! The bug STM exists to prevent: a lock-ordering deadlock.
//!
//! Runs the same bank-transfer workload three ways:
//!
//! 1. **Per-account locks, taken in transfer order** (lock `from`, then
//!    `to`). This is the "obvious" code. Two transfers in opposite
//!    directions each grab their first lock and wait forever for the
//!    other's. A watchdog notices that no transfer has completed for a while
//!    and reports the deadlock.
//! 2. **Per-account locks, taken in index order** (always the lower index
//!    first). Correct, but only because the programmer remembered a global
//!    rule that the compiler cannot check.
//! 3. **STM.** The transfer just reads and writes both accounts; the library
//!    locks the write-set in a fixed order internally, so the caller cannot
//!    get it wrong.
//!
//! ```text
//! cargo run --release --example deadlock_demo
//! ```

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use stm::{TVar, atomically};

const ACCOUNTS: usize = 4;
const THREADS: usize = 8;
const TRANSFERS_PER_THREAD: u64 = 20_000;
const STARTING_BALANCE: i64 = 1_000;
/// No completed transfer for this long counts as a deadlock.
const STALL: Duration = Duration::from_secs(2);

/// Deterministic pseudo-random transfers, so every strategy gets the same work.
fn transfers(seed: u64) -> impl Iterator<Item = (usize, usize, i64)> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..TRANSFERS_PER_THREAD).map(move |_| {
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let from = (next() % ACCOUNTS as u64) as usize;
        let to = (from + 1 + (next() % (ACCOUNTS as u64 - 1)) as usize) % ACCOUNTS;
        (from, to, (next() % 50) as i64 + 1)
    })
}

/// How a run ended.
enum Outcome {
    Finished { elapsed: Duration, total: i64 },
    Deadlocked { completed: u64, stuck: usize },
}

/// Run `work(thread_index, progress)` on `THREADS` detached threads and watch
/// `progress`. Threads are detached (not scoped) because a deadlocked thread
/// can never be joined; the process exits at the end regardless.
fn supervise(
    work: impl Fn(u64, &AtomicU64) + Send + Sync + 'static,
    total: impl Fn() -> i64,
) -> Outcome {
    let work = Arc::new(work);
    let progress = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    for t in 0..THREADS {
        let (work, progress, finished) = (work.clone(), progress.clone(), finished.clone());
        thread::spawn(move || {
            work(t as u64 + 1, &progress);
            finished.fetch_add(1, Ordering::SeqCst);
        });
    }

    let mut last_progress = 0;
    let mut last_change = Instant::now();
    loop {
        thread::sleep(Duration::from_millis(50));
        if finished.load(Ordering::SeqCst) == THREADS as u64 {
            return Outcome::Finished {
                elapsed: started.elapsed(),
                total: total(),
            };
        }
        let now = progress.load(Ordering::SeqCst);
        if now != last_progress {
            last_progress = now;
            last_change = Instant::now();
        } else if last_change.elapsed() > STALL {
            return Outcome::Deadlocked {
                completed: now,
                stuck: THREADS - finished.load(Ordering::SeqCst) as usize,
            };
        }
    }
}

fn report(name: &str, outcome: Outcome) {
    let expected = STARTING_BALANCE * ACCOUNTS as i64;
    let planned = TRANSFERS_PER_THREAD * THREADS as u64;
    match outcome {
        Outcome::Finished { elapsed, total } => println!(
            "  {name:<34} finished {planned} transfers in {:>6.1} ms, total {total} (expected {expected}) {}",
            elapsed.as_secs_f64() * 1e3,
            if total == expected { "OK" } else { "WRONG" }
        ),
        Outcome::Deadlocked { completed, stuck } => println!(
            "  {name:<34} DEADLOCK: no progress for {} s after {completed} of {planned} transfers; {stuck} threads stuck waiting on a lock",
            STALL.as_secs()
        ),
    }
}

fn main() {
    println!("{THREADS} threads x {TRANSFERS_PER_THREAD} transfers over {ACCOUNTS} accounts\n");

    // 1. Lock `from` then `to`. Deadlocks when two transfers go in opposite
    //    directions at the same time.
    let accounts: Arc<Vec<Mutex<i64>>> = Arc::new(
        (0..ACCOUNTS)
            .map(|_| Mutex::new(STARTING_BALANCE))
            .collect(),
    );
    let shared = accounts.clone();
    let outcome = supervise(
        move |seed, progress| {
            for (from, to, amount) in transfers(seed) {
                let mut a = shared[from].lock();
                // Hold the first lock a moment, as real code doing more work
                // would; this makes the deadlock show up within milliseconds.
                thread::yield_now();
                let mut b = shared[to].lock();
                *a -= amount;
                *b += amount;
                progress.fetch_add(1, Ordering::Relaxed);
            }
        },
        // Never called on deadlock: the locks are held forever.
        move || accounts.iter().map(|a| *a.lock()).sum(),
    );
    report("locks in transfer order", outcome);

    // 2. Always lock the lower index first.
    let accounts: Arc<Vec<Mutex<i64>>> = Arc::new(
        (0..ACCOUNTS)
            .map(|_| Mutex::new(STARTING_BALANCE))
            .collect(),
    );
    let shared = accounts.clone();
    let outcome = supervise(
        move |seed, progress| {
            for (from, to, amount) in transfers(seed) {
                let (first, second) = if from < to { (from, to) } else { (to, from) };
                let mut a = shared[first].lock();
                thread::yield_now();
                let mut b = shared[second].lock();
                if from < to {
                    *a -= amount;
                    *b += amount;
                } else {
                    *b -= amount;
                    *a += amount;
                }
                progress.fetch_add(1, Ordering::Relaxed);
            }
        },
        move || accounts.iter().map(|a| *a.lock()).sum(),
    );
    report("locks in index order", outcome);

    // 3. STM: no lock ordering in user code at all.
    let accounts: Arc<Vec<TVar<i64>>> =
        Arc::new((0..ACCOUNTS).map(|_| TVar::new(STARTING_BALANCE)).collect());
    let shared = accounts.clone();
    let outcome = supervise(
        move |seed, progress| {
            for (from, to, amount) in transfers(seed) {
                atomically(|tx| {
                    let a = tx.read(&shared[from])?;
                    thread::yield_now();
                    let b = tx.read(&shared[to])?;
                    tx.write(&shared[from], a - amount);
                    tx.write(&shared[to], b + amount);
                    Ok(())
                });
                progress.fetch_add(1, Ordering::Relaxed);
            }
        },
        move || atomically(|tx| accounts.iter().map(|a| tx.read(a)).sum()),
    );
    report("STM (atomically)", outcome);

    // Deadlocked threads from run 1 are still blocked; exit without them.
    std::process::exit(0);
}
