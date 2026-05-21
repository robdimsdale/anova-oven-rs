//! Pure-logic core for `anova-oven-pico`.
//!
//! Modules here contain decisions, not effects: no HTTP, no GPIO, no LCD I/O,
//! no logging. Everything is unit-testable on a host machine via plain
//! `cargo test`. The bin crate consumes these modules and wraps them in the
//! embassy/embassy-rp/cyw43 hardware glue.
//!
//! See [docs/pico-review.md](../../docs/pico-review.md) §5.1 for the rationale.

#![no_std]

extern crate alloc;

pub mod api;
pub mod encoder;
pub mod fsm;
pub mod reset;
pub mod scheduler;
