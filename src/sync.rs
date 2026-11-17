//! Synchronisation primitives, swappable for `loom`'s model-checked versions.
//!
//! Normal builds use `std` atomics and `parking_lot::Mutex`. When compiled
//! with `RUSTFLAGS="--cfg loom"`, the same names resolve to `loom`'s
//! instrumented types instead, so `tests/loom.rs` can exhaustively explore
//! thread interleavings of the real commit protocol rather than a copy of it.
//!
//! Only the primitives that carry the algorithm's correctness argument (the
//! clock, the per-`TVar` commit lock, and the value/version mutex) go through
//! this module. Reference counting (`Arc`) and the `TVar` id counter use `std`
//! directly: they are not part of the protocol being verified.

#[cfg(not(loom))]
pub(crate) use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(loom)]
pub(crate) use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(not(loom))]
pub(crate) use parking_lot::Mutex;

/// `loom::sync::Mutex` returns a `LockResult` like `std`'s. This wrapper gives
/// it the same infallible `lock()` signature as `parking_lot`, so the rest of
/// the crate is written once.
#[cfg(loom)]
pub(crate) struct Mutex<T>(loom::sync::Mutex<T>);

#[cfg(loom)]
pub(crate) type MutexGuard<'a, T> = loom::sync::MutexGuard<'a, T>;

#[cfg(loom)]
impl<T> Mutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Mutex(loom::sync::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap()
    }
}

/// Hint that we are busy-waiting. Under `loom` this must be a real yield,
/// otherwise the model checker would explore an unbounded spin.
#[inline]
pub(crate) fn spin_loop() {
    #[cfg(not(loom))]
    std::hint::spin_loop();
    #[cfg(loom)]
    loom::thread::yield_now();
}

/// Give up the rest of this time slice.
#[inline]
pub(crate) fn yield_now() {
    #[cfg(not(loom))]
    std::thread::yield_now();
    #[cfg(loom)]
    loom::thread::yield_now();
}
