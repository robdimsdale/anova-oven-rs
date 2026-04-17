//! Processor skeletons for the async event-loop architecture.
//!
//! Phase 1 scaffolding: each module exposes a `run` function that will drive
//! an independent async loop owning its processor's private state. The loops
//! are intentionally empty in Phase 1 — legacy `main.rs` behavior still
//! handles all work until later phases swap the implementations in.

pub mod firestore;
pub mod http;
pub mod state_machine;
pub mod ws;
