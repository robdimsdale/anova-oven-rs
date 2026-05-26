//! Pure-logic core for the Anova-oven firmware.
//!
//! Modules here contain decisions, not effects: no HTTP, no GPIO, no LCD I/O,
//! no logging, no chip-specific MMIO. Everything is unit-testable on a host
//! machine via plain `cargo test`. The bin crate consumes these modules and
//! wraps them in the embassy executor + a chip-specific HAL + a network/radio
//! driver — today that's `embassy-rp` and `cyw43` on the Pico W, but this
//! crate is intended to stay portable across embassy-supported MCU families
//! (RP2040/RP2350, STM32, nRF, ESP32-via-esp-hal, …). The hard dependency on
//! the embassy ecosystem (`embassy-time` types) is deliberate; the chip
//! family is not.
//!
//! See [docs/pico-review.md](../../docs/pico-review.md) §5.1 for the rationale.

#![no_std]

extern crate alloc;

pub mod api;
pub mod encoder;
pub mod fsm;
pub mod persist_data;
pub mod reset;
pub mod scheduler;
