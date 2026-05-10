//! Consumer-release certification tier per [ADR-0012](../../../../docs/adrs/0012-consumer-release-certification.md).
//!
//! Each per-consumer module asserts the typed shape that consumer's contract
//! depends on, sourced from a closed gap row in
//! [`closing-the-zaino-surface-gap.md`](../../../../docs/reference/closing-the-zaino-surface-gap.md).
//! Parity here means "Zinder serves the consumer-expected shape", not byte-equivalence
//! with Zaino (which Zinder deliberately refuses for the documented anti-patterns).
//!
//! closes: G11

mod explorers;
mod lightwalletd_operators;
mod zallet;
mod zashi;
