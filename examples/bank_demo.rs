//! Live bank-transfer demo with a terminal dashboard.
//!
//! Dozens of threads move money between a handful of accounts, every
//! transfer inside a transaction. While they run, the dashboard shows:
//!
//! * throughput (committed transactions per second) and its recent history,
//! * the conflict rate (attempts thrown away and retried),
//! * a grid with one cell per thread, coloured by that thread's conflict
//!   rate over the last refresh, and
//! * the invariant: an auditor thread sums every account inside its own
//!   transaction, over and over, and checks the total never changes.
//!
//! ```text
//! cargo run --release --example bank_demo                      # 64 threads, 10 accounts, 10 s
//! cargo run --release --example bank_demo -- --threads 100 --accounts 2 --seconds 20
//! cargo run --release --example bank_demo -- --plain           # no live redraw (for logs/CI)
//! cargo run --release --example bank_demo -- --snapshot f.ans # also save the last live frame
//! ```
//!
//! Fewer accounts means more contention: try `--accounts 2` and watch the
//! grid turn red while the total stays exactly the same.

use rand::random_range;
use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use stm::{TVar, TxError, TxStats, thread_stats, try_atomically};

const STARTING_BALANCE: i64 = 1_000;
const MAX_TRANSFER: i64 = 200;
const REFRESH: Duration = Duration::from_millis(250);
const HISTORY: usize = 40;

struct Config {
    threads: usize,
    accounts: usize,
    seconds: u64,
    plain: bool,
    snapshot: Option<String>,
}

fn parse_args() -> Config {
    let mut config = Config {
        threads: 64,
        accounts: 10,
        seconds: 10,
        plain: !std::io::stdout().is_terminal(),
        snapshot: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .unwrap_or_else(|| usage(&format!("{name} needs a value")))
        };
        match arg.as_str() {
            "--threads" => config.threads = number(&value("--threads")),
            "--accounts" => config.accounts = number(&value("--accounts")),
            "--seconds" => config.seconds = number(&value("--seconds")) as u64,
            "--plain" => config.plain = true,
            "--snapshot" => config.snapshot = Some(value("--snapshot")),
            "-h" | "--help" => usage(""),
            other => usage(&format!("unknown argument {other}")),
        }
    }
    if config.accounts < 2 || config.threads == 0 {
        usage("need at least 2 accounts and 1 thread");
    }
    config
}

fn number(text: &str) -> usize {
    text.parse()
        .unwrap_or_else(|_| usage(&format!("not a number: {text}")))
}

fn usage(problem: &str) -> ! {
    if !problem.is_empty() {
        eprintln!("error: {problem}");
    }
    eprintln!(
        "usage: bank_demo [--threads N] [--accounts N] [--seconds N] [--plain] [--snapshot FILE]"
    );
    std::process::exit(if problem.is_empty() { 0 } else { 2 });
}

/// One worker's published counters, on its own cache line so workers never
/// slow each other down by writing neighbouring slots (false sharing).
#[repr(align(128))]
#[derive(Default)]
struct Slot {
    commits: AtomicU64,
    conflicts: AtomicU64,
    rejected: AtomicU64,
}

impl Slot {
    fn publish(&self, stats: TxStats) {
        self.commits.store(stats.commits, Ordering::Relaxed);
        self.conflicts.store(stats.conflicts(), Ordering::Relaxed);
        self.rejected.store(stats.user_aborts, Ordering::Relaxed);
    }

    fn read(&self) -> (u64, u64, u64) {
        (
            self.commits.load(Ordering::Relaxed),
            self.conflicts.load(Ordering::Relaxed),
            self.rejected.load(Ordering::Relaxed),
        )
    }
}

#[derive(Default)]
struct Audit {
    passes: AtomicU64,
    violations: AtomicU64,
}

fn transfer(from: &TVar<i64>, to: &TVar<i64>, amount: i64) -> Result<(), ()> {
    try_atomically(|tx| {
        let from_balance = tx.read(from)?;
        if from_balance < amount {
            return Err(TxError::Failed(()));
        }
        let to_balance = tx.read(to)?;
        tx.write(from, from_balance - amount);
        tx.write(to, to_balance + amount);
        Ok(())
    })
}

fn total(accounts: &[TVar<i64>]) -> i64 {
    stm::atomically(|tx| accounts.iter().map(|a| tx.read(a)).sum())
}

fn main() {
    let config = parse_args();
    let accounts: Vec<TVar<i64>> = (0..config.accounts)
        .map(|_| TVar::new(STARTING_BALANCE))
        .collect();
    let expected = STARTING_BALANCE * config.accounts as i64;
    let slots: Vec<Slot> = (0..config.threads).map(|_| Slot::default()).collect();
    let audit = Audit::default();
    let stop = AtomicBool::new(false);
    let started = Instant::now();

    let mut dashboard = Dashboard::new(&config, expected);

    let live_frame = thread::scope(|scope| {
        for slot in &slots {
            let (accounts, stop) = (&accounts, &stop);
            scope.spawn(move || {
                stm::stats::reset_thread_stats();
                while !stop.load(Ordering::Relaxed) {
                    let from = random_range(0..accounts.len());
                    let mut to = random_range(0..accounts.len() - 1);
                    if to >= from {
                        to += 1;
                    }
                    let amount = random_range(1..=MAX_TRANSFER);
                    let _ = transfer(&accounts[from], &accounts[to], amount);
                    slot.publish(thread_stats());
                }
            });
        }

        let (accounts, stop, audit) = (&accounts, &stop, &audit);
        scope.spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if total(accounts) == expected {
                    audit.passes.fetch_add(1, Ordering::Relaxed);
                } else {
                    audit.violations.fetch_add(1, Ordering::Relaxed);
                }
            }
        });

        let deadline = started + Duration::from_secs(config.seconds);
        let mut last_live = String::new();
        while Instant::now() < deadline {
            thread::sleep(REFRESH);
            last_live = dashboard.frame(&slots, audit, accounts, started.elapsed(), false);
        }
        stop.store(true, Ordering::Relaxed);
        last_live
    });

    // Every thread has stopped: one last, definitive frame.
    dashboard.frame(&slots, &audit, &accounts, started.elapsed(), true);
    if let Some(path) = &config.snapshot {
        // The last live frame, colour codes included, for screenshots.
        std::fs::write(path, &live_frame).expect("write snapshot");
        eprintln!("snapshot of the last live frame written to {path}");
    }

    let final_total = total(&accounts);
    if final_total != expected || audit.violations.load(Ordering::Relaxed) != 0 {
        eprintln!("INVARIANT VIOLATED: final total {final_total}, expected {expected}");
        std::process::exit(1);
    }
}

struct Dashboard {
    threads: usize,
    accounts: usize,
    expected: i64,
    plain: bool,
    previous: Vec<(u64, u64, u64)>,
    last_elapsed: Duration,
    history: Vec<f64>,
    peak: f64,
    frames: u64,
}

impl Dashboard {
    fn new(config: &Config, expected: i64) -> Self {
        if !config.plain {
            // Hide the cursor and clear the screen once.
            print!("\x1b[?25l\x1b[2J");
        }
        Dashboard {
            threads: config.threads,
            accounts: config.accounts,
            expected,
            plain: config.plain,
            previous: vec![(0, 0, 0); config.threads],
            last_elapsed: Duration::ZERO,
            history: Vec::new(),
            peak: 0.0,
            frames: 0,
        }
    }

    /// Render one frame and return it (with colour codes).
    fn frame(
        &mut self,
        slots: &[Slot],
        audit: &Audit,
        accounts: &[TVar<i64>],
        elapsed: Duration,
        last: bool,
    ) -> String {
        let now: Vec<_> = slots.iter().map(Slot::read).collect();
        let interval = (elapsed - self.last_elapsed).as_secs_f64().max(1e-9);
        let (mut commits, mut conflicts, mut rejected) = (0, 0, 0);
        let (mut d_commits, mut d_conflicts) = (0, 0);
        let mut cells = Vec::with_capacity(self.threads);
        for (&(c, x, r), &(pc, px, _)) in now.iter().zip(&self.previous) {
            commits += c;
            conflicts += x;
            rejected += r;
            d_commits += c - pc;
            d_conflicts += x - px;
            cells.push((c - pc, x - px));
        }
        let rate = d_commits as f64 / interval;
        if !last {
            self.history.push(rate);
            if self.history.len() > HISTORY {
                self.history.remove(0);
            }
            self.peak = self.peak.max(rate);
        }
        self.previous = now;
        self.last_elapsed = elapsed;

        let total_now = total(accounts);
        let passes = audit.passes.load(Ordering::Relaxed);
        let violations = audit.violations.load(Ordering::Relaxed);
        let attempts = commits + conflicts + rejected;
        let overall_conflict = conflicts as f64 / attempts.max(1) as f64;
        let recent_conflict = d_conflicts as f64 / (d_commits + d_conflicts).max(1) as f64;

        let mut out = String::new();
        let _ = writeln!(
            out,
            "\x1b[1mSTM bank demo\x1b[0m  {} threads, {} accounts, {:.1} s{}",
            self.threads,
            self.accounts,
            elapsed.as_secs_f64(),
            if last { "  (finished)" } else { "" }
        );
        let _ = writeln!(out);
        let average = commits as f64 / elapsed.as_secs_f64().max(1e-9) / 1e6;
        if last {
            let _ = writeln!(
                out,
                "  committed   {:>12}   ({average:.2} M/s average)",
                group(commits)
            );
        } else {
            let _ = writeln!(
                out,
                "  committed   {:>12}   ({:.2} M/s now, {average:.2} M/s average)",
                group(commits),
                rate / 1e6
            );
        }
        let _ = writeln!(
            out,
            "  conflicts   {:>12}   ({:.1}% of attempts retried{})",
            group(conflicts),
            overall_conflict * 100.0,
            if last {
                String::new()
            } else {
                format!(
                    "; {:.1}% in the last {} ms",
                    recent_conflict * 100.0,
                    REFRESH.as_millis()
                )
            }
        );
        let _ = writeln!(
            out,
            "  rejected    {:>12}   (insufficient funds, rolled back)",
            group(rejected)
        );
        let _ = writeln!(out);
        let status = if total_now == self.expected && violations == 0 {
            "\x1b[32mOK\x1b[0m"
        } else {
            "\x1b[31mVIOLATED\x1b[0m"
        };
        let _ = writeln!(
            out,
            "  invariant   total = {} (expected {})  {status}   {} audits, {} violations",
            group(total_now as u64),
            group(self.expected as u64),
            group(passes),
            violations
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  throughput  {}  peak {:.2} M/s",
            sparkline(&self.history, self.peak),
            self.peak / 1e6
        );
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "  threads (colour = conflict rate in the last refresh: \x1b[32m■\x1b[0m <10%  \x1b[33m■\x1b[0m <40%  \x1b[31m■\x1b[0m >=40%)"
        );
        let columns = 16;
        for row in cells.chunks(columns) {
            out.push_str("   ");
            for &(c, x) in row {
                let share = x as f64 / (c + x).max(1) as f64;
                let colour = if share < 0.10 {
                    32
                } else if share < 0.40 {
                    33
                } else {
                    31
                };
                let _ = write!(out, " \x1b[{colour}m■\x1b[0m");
            }
            out.push('\n');
        }

        if self.plain {
            // Plain mode: print every 4th frame (about once a second) plus
            // the last one, with no cursor movement.
            self.frames += 1;
            if last || self.frames % 4 == 0 {
                println!("{}", strip_ansi(&out));
            }
        } else {
            print!("\x1b[H{out}\x1b[J");
            if last {
                print!("\x1b[?25h");
            }
        }
        let _ = std::io::stdout().flush();
        out
    }
}

/// Unicode block sparkline of `values`, scaled to `peak`.
fn sparkline(values: &[f64], peak: f64) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut line: String = values
        .iter()
        .map(|&v| {
            let level = if peak > 0.0 { v / peak } else { 0.0 };
            BARS[((level * 7.0).round() as usize).min(7)]
        })
        .collect();
    while line.chars().count() < HISTORY {
        line.insert(0, ' ');
    }
    line
}

/// 1234567 -> "1,234,567".
fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Remove ANSI escape sequences (colours, cursor movement).
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip to the end of the CSI sequence (a letter).
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}
