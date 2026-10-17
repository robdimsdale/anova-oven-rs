//! Shared message contracts for the processor-based runtime.
//!
//! Phase 1 scaffolding: these types define the typed channels between the
//! HTTP, WebSocket, store (Firestore), and state-machine processors. They are not yet
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
    /// Not possible in this mode: the cloud refuses it, or it isn't wired up.
    Unsupported(String),
    /// Not possible in the oven's current state, e.g. no next stage.
    Conflict(String),
    Store(StoreError),
    Internal(String),
}

impl std::fmt::Display for SmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SmError::NotCooking => write!(f, "not cooking"),
            SmError::Disconnected => write!(f, "oven disconnected"),
            SmError::RecipeNotFound(id) => write!(f, "recipe not found: {id}"),
            SmError::Unsupported(e) => write!(f, "unsupported: {e}"),
            SmError::Conflict(e) => write!(f, "{e}"),
            SmError::Store(e) => write!(f, "store: {e}"),
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
    /// Start a cook from stages given with the request, not a saved recipe.
    StartManualCook {
        title: String,
        stages: Vec<Stage>,
        reply: oneshot::Sender<Result<(), SmError>>,
    },
    StopCook {
        reply: oneshot::Sender<Result<(), SmError>>,
    },
    /// A control the cloud refused or never offered; local mode only.
    Control {
        request: ControlRequest,
        reply: oneshot::Sender<Result<(), SmError>>,
    },
}

/// A command the oven didn't carry out: refused, not acked in time, or sent
/// with no oven connected. Published to Home Assistant as `command_failed`.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandFailure {
    /// `start`, `stop`, `next_stage`, `timer`, `temperature`, `lamp` or
    /// `temperature_unit`.
    pub command: String,
    pub reason: String,
}

impl ControlRequest {
    /// The name used for this control in `command_failed` events.
    pub fn name(&self) -> &'static str {
        match self {
            ControlRequest::NextStage => "next_stage",
            ControlRequest::Timer { .. } => "timer",
            ControlRequest::Temperature { .. } => "temperature",
            ControlRequest::Lamp { .. } => "lamp",
            ControlRequest::TemperatureUnit { .. } => "temperature_unit",
        }
    }
}

/// What a client asks of the running cook.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlRequest {
    /// Start the stage after the active one, whether or not it waits for the
    /// user.
    NextStage,
    /// Restart the active stage's countdown from `secs`.
    Timer {
        secs: u64,
    },
    /// Change the active stage's setpoint, in its own bulb mode.
    Temperature {
        celsius: f32,
    },
    Lamp {
        on: bool,
    },
    /// The oven's display unit: `"C"` or `"F"`.
    TemperatureUnit {
        unit: String,
    },
}

/// A command for the oven itself, as the device side sends it.
#[derive(Clone, Debug)]
pub enum OvenControl {
    StartStage {
        stage_id: String,
    },
    SetTimer {
        secs: u64,
    },
    SetTemperatureBulbs {
        mode: String,
        celsius: f32,
    },
    SetLamp {
        on: bool,
    },
    SetTemperatureUnit {
        unit: String,
    },
    /// The whole stage list, already in the oven's format.
    UpdateCookStages {
        stages: Vec<serde_json::Value>,
    },
}

#[derive(Debug)]
pub enum StateMachineEvent {
    Ws(WsEvent),
    Store(StoreEvent),
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
    /// Local mode only.
    Control {
        request_id: String,
        control: OvenControl,
    },
}

/// The running cook as the oven reports it in its `cook` block.
#[derive(Clone, Debug)]
pub struct OvenCook {
    pub cook_id: String,
    /// The oven's own copy of the stages, which can change mid-cook.
    pub stages: Vec<Stage>,
    /// The same stages as the oven sent them, for edits sent back with
    /// `CMD_APO_UPDATE_COOK_STAGES` without losing what [`Stage`] can't hold.
    pub raw_stages: Vec<serde_json::Value>,
    /// Seconds since the cook started.
    pub seconds_elapsed: u64,
}

#[derive(Debug)]
pub enum WsEvent {
    Connected,
    Disconnected,
    ApoState {
        status: OvenStatus,
        /// The oven's `cook` block, when it has one.
        cook: Option<OvenCook>,
    },
    CookerDiscovered {
        cooker_id: String,
    },
    CommandAck {
        request_id: String,
        status: String,
    },
    ParseError {
        detail: String,
    },
}

// ─── Store processor ─────────────────────────────────────────────────────────

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

/// Store-side typed error, isolated from `FirestoreError` so the state
/// machine does not have to depend on transport details.
#[derive(Debug)]
pub enum StoreError {
    Unauthorized,
    Timeout,
    /// The store doesn't keep this; the state machine keeps what it has.
    Unsupported,
    Other(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Unauthorized => write!(f, "unauthorized"),
            StoreError::Timeout => write!(f, "timeout"),
            StoreError::Unsupported => write!(f, "not supported by this store"),
            StoreError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<FirestoreError> for StoreError {
    fn from(value: FirestoreError) -> Self {
        match value {
            FirestoreError::Unauthorized => StoreError::Unauthorized,
            FirestoreError::Other(e) => StoreError::Other(e.to_string()),
        }
    }
}

#[derive(Debug)]
pub enum StoreCommand {
    RefreshRecipes,
    RefreshHistory {
        reason: HistoryReason,
    },
    FetchCurrentCook {
        reason: CurrentCookReason,
        /// The oven's own view of the cook, from the latest state. The
        /// local store builds the current cook from it; Firestore ignores it.
        oven_cook: Option<OvenCook>,
    },
    /// Sent on every oven report, with the oven's cook if it has one. The
    /// local store reconciles its journal of the cook in flight against it
    /// and records cooks that have ended; Firestore ignores it, since Anova's
    /// cloud keeps its own history.
    OvenReported {
        oven_cook: Option<OvenCook>,
    },
    /// The server started a cook. The local store journals it; Firestore
    /// tags the cook with its recipe.
    PatchCookRecipeRef {
        cook_id: String,
        from: StartedFrom,
    },
    ResolveManualCookTitle {
        cook: CurrentCook,
    },
}

/// What a cook the server started was made from.
#[derive(Clone, Debug, PartialEq)]
pub enum StartedFrom {
    /// A saved recipe, by ID.
    Recipe(String),
    /// Stages given with the start request rather than a saved recipe.
    Manual {
        title: String,
        cook_stage_count: usize,
    },
}

impl StartedFrom {
    pub fn recipe_id(&self) -> Option<&str> {
        match self {
            StartedFrom::Recipe(id) => Some(id),
            StartedFrom::Manual { .. } => None,
        }
    }
}

#[derive(Debug)]
pub enum StoreEvent {
    RecipesRefreshed(Result<Vec<Recipe>, StoreError>),
    HistoryRefreshed(Result<Vec<HistoryEntry>, StoreError>),
    CurrentCookFetched {
        reason: CurrentCookReason,
        result: Result<Option<CurrentCook>, StoreError>,
    },
    RecipeRefPatched {
        cook_id: String,
        recipe_id: Option<String>,
        result: Result<(), StoreError>,
    },
    ManualCookTitleResolved {
        cook_key: String,
        result: Result<Option<(String, String)>, StoreError>,
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
