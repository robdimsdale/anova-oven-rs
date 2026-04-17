//! Shared message contracts for the processor-based runtime.
//!
//! Phase 1 scaffolding: these types define the typed channels between the
//! HTTP, WebSocket, Firestore, and state-machine processors. They are not yet
//! wired into `main.rs` — the legacy `AppState` code path remains active.

#![allow(dead_code, clippy::enum_variant_names, clippy::large_enum_variant)]

use anova_oven_api::{CurrentCook, HistoryEntry, OvenStatus, Recipe, Stage};
use tokio::sync::oneshot;

use crate::firestore::FirestoreError;

// ─── State-machine facing ────────────────────────────────────────────────────

pub type StatusDto = OvenStatus;
pub type CurrentCookDto = CurrentCook;
pub type RecipeDto = Recipe;
pub type HistoryEntryDto = HistoryEntry;

#[derive(Debug)]
pub enum SmError {
    NotCooking,
    Disconnected,
    RecipeNotFound(String),
    Firestore(FirestoreTaskError),
    Internal(String),
}

impl std::fmt::Display for SmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmError::NotCooking => write!(f, "not cooking"),
            SmError::Disconnected => write!(f, "oven disconnected"),
            SmError::RecipeNotFound(id) => write!(f, "recipe not found: {id}"),
            SmError::Firestore(e) => write!(f, "firestore: {e}"),
            SmError::Internal(e) => write!(f, "internal: {e}"),
        }
    }
}

impl std::error::Error for SmError {}

#[derive(Debug)]
pub enum StateMachineCommand {
    GetStatus {
        reply: oneshot::Sender<Result<StatusDto, SmError>>,
    },
    GetCurrentCook {
        reply: oneshot::Sender<Result<Option<CurrentCookDto>, SmError>>,
    },
    GetRecipes {
        reply: oneshot::Sender<Result<Vec<RecipeDto>, SmError>>,
    },
    GetHistory {
        reply: oneshot::Sender<Result<Vec<HistoryEntryDto>, SmError>>,
    },
    RefreshRecipes {
        reply: oneshot::Sender<Result<Vec<RecipeDto>, SmError>>,
    },
    StartCook {
        recipe_id: String,
        reply: oneshot::Sender<Result<(), SmError>>,
    },
    StopCook {
        reply: oneshot::Sender<Result<(), SmError>>,
    },
}

#[derive(Debug)]
pub enum StateMachineEvent {
    Ws(WsEvent),
    Firestore(FirestoreEvent),
    Tick(TickKind),
}

#[derive(Clone, Copy, Debug)]
pub enum TickKind {
    CurrentCookRefresh,
    RecipesRefresh,
    HistoryRefresh,
    PendingTimeout,
}

// ─── WebSocket processor ─────────────────────────────────────────────────────

#[derive(Debug)]
pub enum WsCommand {
    SendStop {
        request_id: String,
    },
    SendStart {
        request_id: String,
        cook_id: String,
        recipe_id: String,
        stages: Vec<Stage>,
    },
}

#[derive(Debug)]
pub enum WsEvent {
    Connected,
    Disconnected,
    ApoState(OvenStatus),
    CookerDiscovered { cooker_id: String },
    CommandAck { request_id: String, status: String },
    ParseError { detail: String },
}

// ─── Firestore processor ─────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum HistoryReason {
    Periodic,
    Startup,
    WsIdleToCooking,
    WsCookToIdle,
    WsStageTransition,
}

#[derive(Clone, Copy, Debug)]
pub enum CurrentCookReason {
    Periodic,
    Startup,
    WsIdleToCooking,
    WsConnectedCookingState,
    WsStageTransition,
    StartCommandAccepted,
}

/// Firestore-side typed error, isolated from `FirestoreError` so the state
/// machine does not have to depend on transport details.
#[derive(Debug)]
pub enum FirestoreTaskError {
    Unauthorized,
    Timeout,
    Other(String),
}

impl std::fmt::Display for FirestoreTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FirestoreTaskError::Unauthorized => write!(f, "unauthorized"),
            FirestoreTaskError::Timeout => write!(f, "timeout"),
            FirestoreTaskError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FirestoreTaskError {}

impl From<FirestoreError> for FirestoreTaskError {
    fn from(value: FirestoreError) -> Self {
        match value {
            FirestoreError::Unauthorized => FirestoreTaskError::Unauthorized,
            FirestoreError::Other(e) => FirestoreTaskError::Other(e.to_string()),
        }
    }
}

#[derive(Debug)]
pub enum FirestoreCommand {
    RefreshRecipes,
    RefreshHistory { reason: HistoryReason },
    FetchCurrentCook { reason: CurrentCookReason },
    PatchCookRecipeRef { cook_id: String, recipe_id: String },
    ResolveManualCookTitle { cook: CurrentCook },
}

#[derive(Debug)]
pub enum FirestoreEvent {
    RecipesRefreshed(Result<Vec<Recipe>, FirestoreTaskError>),
    HistoryRefreshed(Result<Vec<HistoryEntry>, FirestoreTaskError>),
    CurrentCookFetched {
        reason: CurrentCookReason,
        result: Result<Option<CurrentCook>, FirestoreTaskError>,
    },
    RecipeRefPatched {
        cook_id: String,
        recipe_id: String,
        result: Result<(), FirestoreTaskError>,
    },
    ManualCookTitleResolved {
        cook_key: String,
        result: Result<Option<(String, String)>, FirestoreTaskError>,
    },
}

// ─── Transition origin (read model) ──────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub enum TransitionOrigin {
    LocalCommand,
    WsExternal,
    WsReconciliation,
    AckConfirmed,
    TimeoutRecovery,
    StartupSync,
}
