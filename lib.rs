//! Software Transactional Memory (STM) library, built on the TL2 algorithm.
//!
//! See `README.md` for the project overview and planned usage, and
//! `ARCHITECTURE.md` for design-decision notes as they're written.
//!
//! Module map:
//! - [`clock`]: the global version clock.
//! - [`tvar`]: `TVar<T>`, the transactional variable type.
//! - [`transaction`]: the transaction context, commit protocol, and the
//!   `atomically` entry point.
//!
//! # Example
//!
//! ```
//! use stm::{atomically, TVar};
//!
//! let from = TVar::new(100);
//! let to = TVar::new(0);
//!
//! atomically(|tx| {
//!     let a = tx.read(&from)?;
//!     let b = tx.read(&to)?;
//!     tx.write(&from, a - 30);
//!     tx.write(&to, b + 30);
//!     Ok(())
//! });
//!
//! assert_eq!(atomically(|tx| Ok(tx.read(&from)? + tx.read(&to)?)), 100);
//! ```

pub mod clock;
pub mod transaction;
pub mod tvar;

// Re-export the pieces most callers will actually reach for, so library
// users can write `stm::atomically(...)` and `stm::TVar::new(...)`
// instead of the fully qualified paths.
pub use transaction::{Conflict, Transaction, TxError, atomically, try_atomically};
pub use tvar::TVar;
