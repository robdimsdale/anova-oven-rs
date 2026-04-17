//! Read model snapshots published by the state-machine processor.
//!
//! Phase 1 scaffolding: the HTTP processor will eventually consume this
//! snapshot (via `watch`) or issue `StateMachineCommand::Get*` requests for
//! point-in-time reads. The concrete shape will be filled in when the state
//! machine is extracted.

#![allow(dead_code)]

use anova_oven_api::{CurrentCook, HistoryEntry, OvenStatus, Recipe};

use crate::runtime::types::TransitionOrigin;

#[derive(Clone, Debug, Default)]
pub struct ReadModel {
    pub status: Option<OvenStatus>,
    pub current_cook: Option<CurrentCook>,
    pub recipes: Vec<Recipe>,
    pub history: Vec<HistoryEntry>,
    pub last_transition_origin: Option<TransitionOrigin>,
}
