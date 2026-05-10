//! Consumer-release certification tests for the public client contracts.
//!
//! Each per-consumer module asserts the typed shape that consumer's contract
//! depends on. Parity here means "Zinder serves the consumer-expected shape",
//! not byte-equivalence with Zaino (which Zinder deliberately refuses for
//! the documented anti-patterns).

mod explorers;
mod lightwalletd_operators;
mod zallet;
mod zashi;
