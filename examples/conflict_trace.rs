//! A conflict and a retry, step by step, with two threads.
//!
//! Two threads each run ONE transaction that reads a shared counter, pauses,
//! and writes back counter + 1. The pause guarantees their attempts overlap,
//! so one of them must lose: its read is invalidated by the other's commit,
//! and the library re-runs it. The output narrates every attempt.
//!
//! (A transaction body may run more than once, so printing from inside it is
//! normally a bad idea. Here showing each attempt is the whole point.)
//!
//! ```text
//! cargo run --example conflict_trace
//! cargo run --example conflict_trace --features trace   # plus the library's own log
//! ```

use std::sync::Barrier;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::Duration;
use stm::{TVar, atomically, thread_stats};

fn main() {
    let counter = TVar::new(0);
    let both_read = Barrier::new(2);

    thread::scope(|scope| {
        for (name, delay_ms) in [("A", 50), ("B", 150)] {
            let (counter, both_read) = (&counter, &both_read);
            scope.spawn(move || {
                let attempts = AtomicU32::new(0);
                atomically(|tx| {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    let value = tx.read(counter)?;
                    println!("[{name}] attempt {attempt}: read counter = {value}");
                    if attempt == 1 {
                        // Make sure both first attempts have read before
                        // either commits, then let A finish first.
                        both_read.wait();
                        thread::sleep(Duration::from_millis(delay_ms));
                    }
                    println!("[{name}] attempt {attempt}: writing counter = {}, committing...", value + 1);
                    tx.write(counter, value + 1);
                    Ok(())
                });
                let stats = thread_stats();
                println!(
                    "[{name}] committed after {} attempt(s): {} conflict(s) detected at commit, {} at read",
                    stats.attempts(),
                    stats.validation_failures,
                    stats.read_conflicts
                );
            });
        }
    });

    let final_value = atomically(|tx| tx.read(&counter));
    println!("\nfinal counter = {final_value} (two increments, none lost)");
    assert_eq!(final_value, 2);
}
