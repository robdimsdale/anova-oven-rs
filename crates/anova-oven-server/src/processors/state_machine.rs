//! State machine processor.
//!
//! Owns canonical oven/app state and orchestrates WebSocket + store
//! effects via pure reducers. Phase 4 builds out the transition logic, pending
//! command tracker, and read-model snapshot. The processor is *built* here
//! but not yet wired into `main.rs` — Phase 5 will swap HTTP handlers onto it.
//!
//! Side effects are returned as `SmEffect` values and dispatched by the outer
//! event loop, keeping [`reduce_command`] / [`reduce_event`] free of IO and
//! directly unit-testable.

#![allow(dead_code)]

use anova_oven_api::{CookProgress, CurrentCook, HistoryEntry, OvenStatus, Recipe, Stage};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::read_model::ReadModel;
use crate::runtime::types::{
    CommandFailure, ControlRequest, CurrentCookReason, HistoryReason, OvenControl, OvenCook,
    SmError, StartedFrom, StateMachineCommand, StateMachineEvent, StoreCommand, StoreError,
    StoreEvent, TransitionOrigin, WsCommand, WsEvent,
};

// ─── Canonical state ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct SmState {
    pub connectivity: Connectivity,
    pub lifecycle: Lifecycle,
    pub last_origin: Option<TransitionOrigin>,
    pub pending: PendingTracker,
    pub status: Option<OvenStatus>,
    /// The oven's own view of the running cook, from the latest state.
    pub oven_cook: Option<OvenCook>,
    pub current_cook: Option<CurrentCook>,
    pub cook_progress: Option<CookProgress>,
    pub recipes: Vec<Recipe>,
    pub history: Vec<HistoryEntry>,
    /// Local mode: the server is the oven's cloud. Command acks come from the
    /// oven's own state, sent just before the ack, and the controls Anova's
    /// cloud refused are available.
    pub local: bool,
}

#[derive(Clone, Debug, Default)]
pub enum Connectivity {
    #[default]
    Disconnected,
    Connected {
        cooker_id: Option<String>,
    },
}

impl Connectivity {
    pub fn cooker_id(&self) -> Option<&str> {
        match self {
            Connectivity::Connected {
                cooker_id: Some(id),
            } => Some(id.as_str()),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Lifecycle {
    #[default]
    Idle,
    StartPending,
    Cooking,
    StopPending,
}

#[derive(Clone, Debug, Default)]
pub struct PendingTracker {
    pub start: Option<PendingStart>,
    pub stop: Option<PendingStop>,
    /// Controls sent and not yet acked, as (request id, name). Bounded: an
    /// ack, a failure or [`MAX_PENDING_CONTROLS`] newer ones clear an entry.
    pub controls: Vec<(String, &'static str)>,
}

/// Every control is acked or timed out within seconds, so this only bounds
/// a run of controls whose acks went missing.
const MAX_PENDING_CONTROLS: usize = 16;

#[derive(Clone, Debug)]
pub struct PendingStart {
    pub request_id: String,
    pub first_stage_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PendingStop {
    pub request_id: String,
}

impl SmState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn to_read_model(&self) -> ReadModel {
        ReadModel {
            status: self.status.clone(),
            current_cook: self.current_cook.clone(),
            recipes: self.recipes.clone(),
            history: self.history.clone(),
            last_transition_origin: self.last_origin,
        }
    }

    fn set_lifecycle(&mut self, next: Lifecycle, origin: TransitionOrigin) {
        if self.lifecycle != next {
            debug!(
                target: "sm",
                from = ?self.lifecycle,
                to = ?next,
                origin = ?origin,
                "lifecycle transition"
            );
        }
        self.lifecycle = next;
        self.last_origin = Some(origin);
    }
}

// ─── Effects ─────────────────────────────────────────────────────────────────

/// Side effects a reducer asks the outer loop to perform. Replies to
/// [`StateMachineCommand`] are delivered via the caller's `oneshot` and do not
/// appear here — keeping the effect list focused on external IO and scheduling.
#[derive(Debug)]
pub enum SmEffect {
    Ws(WsCommand),
    Store(StoreCommand),
    PublishReadModel,
    /// Announce a command the oven didn't carry out.
    CommandFailed(CommandFailure),
}

// ─── Reducers ────────────────────────────────────────────────────────────────

/// Reduce a client command into state mutation + effects. The command's
/// `oneshot::Sender` is returned so the outer loop can deliver the result; the
/// reducer itself stays pure over IO.
pub fn reduce_command(state: &mut SmState, cmd: StateMachineCommand) -> CommandOutcome {
    match cmd {
        StateMachineCommand::GetStatus { reply } => {
            CommandOutcome::reply_status(reply, state.status.clone().ok_or(SmError::Disconnected))
        }
        StateMachineCommand::GetCurrentCook { reply } => {
            CommandOutcome::reply_current_cook(reply, Ok(state.current_cook.clone()))
        }
        StateMachineCommand::GetRecipes { reply } => {
            CommandOutcome::reply_recipes(reply, Ok(state.recipes.clone()), Vec::new())
        }
        StateMachineCommand::GetHistory { reply } => {
            CommandOutcome::reply_history(reply, Ok(state.history.clone()))
        }
        StateMachineCommand::RefreshRecipes { reply } => {
            let effects = vec![SmEffect::Store(StoreCommand::RefreshRecipes)];
            CommandOutcome::reply_recipes(reply, Ok(state.recipes.clone()), effects)
        }
        StateMachineCommand::StartCook { recipe_id, reply } => {
            let (result, effects) = start_cook(state, &recipe_id);
            CommandOutcome::reply_unit_with_effects(reply, result, effects)
        }
        StateMachineCommand::StartManualCook {
            title,
            stages,
            reply,
        } => {
            let (result, effects) = start_manual_cook(state, title, stages);
            CommandOutcome::reply_unit_with_effects(reply, result, effects)
        }
        StateMachineCommand::StopCook { reply } => {
            let (result, effects) = stop_cook(state);
            CommandOutcome::reply_unit_with_effects(reply, result, effects)
        }
        StateMachineCommand::Control { request, reply } => {
            let (result, effects) = match control(state, &request) {
                Ok(control) => {
                    let request_id = Uuid::new_v4().to_string();
                    info!(target: "sm", request = ?request, request_id = %request_id, "sending control");
                    let controls = &mut state.pending.controls;
                    controls.push((request_id.clone(), request.name()));
                    if controls.len() > MAX_PENDING_CONTROLS {
                        controls.remove(0);
                    }
                    (
                        Ok(()),
                        vec![SmEffect::Ws(WsCommand::Control {
                            request_id,
                            control,
                        })],
                    )
                }
                Err(e) => (Err(e), Vec::new()),
            };
            CommandOutcome::reply_unit_with_effects(reply, result, effects)
        }
    }
}

/// Reduce an external event into state mutation + effects. No replies here —
/// events originate from the WS / store processors, not client calls.
pub fn reduce_event(state: &mut SmState, evt: StateMachineEvent) -> Vec<SmEffect> {
    match evt {
        StateMachineEvent::Ws(event) => reduce_ws_event(state, event),
        StateMachineEvent::Store(event) => reduce_store_event(state, event),
        StateMachineEvent::Tick(kind) => match kind {
            crate::runtime::types::TickKind::CurrentCookRefresh => {
                if matches!(
                    state.lifecycle,
                    Lifecycle::Cooking | Lifecycle::StartPending
                ) {
                    vec![fetch_current_cook(state, CurrentCookReason::Periodic)]
                } else {
                    Vec::new()
                }
            }
            crate::runtime::types::TickKind::RecipesRefresh => {
                vec![SmEffect::Store(StoreCommand::RefreshRecipes)]
            }
            crate::runtime::types::TickKind::HistoryRefresh => {
                vec![SmEffect::Store(StoreCommand::RefreshHistory {
                    reason: HistoryReason::Periodic,
                })]
            }
            crate::runtime::types::TickKind::PendingTimeout => Vec::new(),
        },
    }
}

fn fetch_current_cook(state: &SmState, reason: CurrentCookReason) -> SmEffect {
    // While a start is pending, the oven's cook is the one being replaced:
    // passing it would have the local store record that cook in place of
    // the one just started.
    let oven_cook = match state.lifecycle {
        Lifecycle::StartPending => None,
        _ => state.oven_cook.clone(),
    };
    SmEffect::Store(StoreCommand::FetchCurrentCook { reason, oven_cook })
}

// ─── Client-command transitions ──────────────────────────────────────────────

fn start_cook(state: &mut SmState, recipe_id: &str) -> (Result<(), SmError>, Vec<SmEffect>) {
    // Guardrails: connected + known cooker id + recipe resolvable.
    if state.connectivity.cooker_id().is_none() {
        return (Err(SmError::Disconnected), Vec::new());
    }

    let recipe = match state.recipes.iter().find(|r| r.id == recipe_id).cloned() {
        Some(r) => r,
        None => return (Err(SmError::RecipeNotFound(recipe_id.into())), Vec::new()),
    };
    let from = StartedFrom::Recipe(recipe.id.clone());
    start(state, recipe, from)
}

/// Start a cook from stages given with the request. The caller has checked
/// them (`local_store::normalize`).
fn start_manual_cook(
    state: &mut SmState,
    title: String,
    stages: Vec<Stage>,
) -> (Result<(), SmError>, Vec<SmEffect>) {
    if state.connectivity.cooker_id().is_none() {
        return (Err(SmError::Disconnected), Vec::new());
    }
    let cook_stage_count = stages.iter().filter(|s| s.kind != "preheat").count();
    let recipe = Recipe {
        id: format!("manual-{}", Uuid::new_v4().simple()),
        title: title.clone(),
        stage_count: cook_stage_count,
        stages,
        source: Default::default(),
    };
    let from = StartedFrom::Manual {
        title,
        cook_stage_count,
    };
    start(state, recipe, from)
}

fn start(
    state: &mut SmState,
    mut recipe: Recipe,
    from: StartedFrom,
) -> (Result<(), SmError>, Vec<SmEffect>) {
    crate::recipe::rewrite_preheat_stage_ids(&mut recipe.stages);

    // Placeholder request-id tracking: the real WS processor will allocate the
    // request id when it serializes the command.
    let request_id = Uuid::new_v4().to_string();

    state.pending.start = Some(PendingStart {
        request_id: request_id.clone(),
        first_stage_id: recipe.stages.first().and_then(|s| s.id.clone()),
    });
    state.set_lifecycle(Lifecycle::StartPending, TransitionOrigin::LocalCommand);

    // Seed current_cook locally so start-next-stage works before the
    // Firestore round-trip completes. The Firestore fetch (triggered on
    // ApoState cooking / ack) will overwrite with the authoritative record.
    let total_stage_count = recipe.stages.len();
    state.current_cook = Some(CurrentCook {
        recipe_title: recipe.title.clone(),
        recipe_id: from.recipe_id().map(String::from),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| format!("local-{}", d.as_secs()))
            .unwrap_or_else(|_| "local".into()),
        stages: recipe.stages.clone(),
        cook_stage_count: recipe.stage_count,
        total_stage_count,
    });

    let cook_id = format!("ios-{}", uuid::Uuid::new_v4());
    let effects = vec![
        SmEffect::Ws(WsCommand::SendStart {
            request_id,
            cook_id: cook_id.clone(),
            recipe_id: recipe.id.clone(),
            stages: recipe.stages.clone(),
        }),
        SmEffect::Store(StoreCommand::PatchCookRecipeRef { cook_id, from }),
    ];

    (Ok(()), effects)
}

/// Turn a client's control request into a command for the oven.
///
/// While a preheat runs, the oven copies the first `SET_*` onto the paired
/// cook stage, so changes to the stages go out as `UPDATE_COOK_STAGES`
/// instead: a temperature edits the preheat itself, and a timer the cook
/// stage after it (a preheat has no timer of its own; the oven shows the
/// next stage's).
fn control(state: &SmState, request: &ControlRequest) -> Result<OvenControl, SmError> {
    if !state.local {
        return Err(SmError::Unsupported(
            "only available in local mode; Anova's cloud refuses or doesn't relay it".into(),
        ));
    }
    if state.connectivity.cooker_id().is_none() {
        return Err(SmError::Disconnected);
    }
    match request {
        ControlRequest::Lamp { on } => return Ok(OvenControl::SetLamp { on: *on }),
        ControlRequest::TemperatureUnit { unit } => {
            return Ok(OvenControl::SetTemperatureUnit { unit: unit.clone() })
        }
        _ => {}
    }

    let status = state
        .status
        .as_ref()
        .filter(|s| s.is_cooking())
        .ok_or(SmError::NotCooking)?;
    let cook = state.oven_cook.as_ref().ok_or(SmError::NotCooking)?;
    let active = status
        .active_stage_index
        .filter(|i| *i < cook.raw_stages.len())
        .ok_or_else(|| SmError::Conflict("the oven hasn't reported its active stage".into()))?;
    let in_preheat = cook.stages.get(active).is_some_and(|s| s.kind == "preheat");

    match request {
        ControlRequest::NextStage => {
            let next = cook
                .stages
                .get(active + 1)
                .ok_or_else(|| SmError::Conflict("the active stage is the last one".into()))?;
            let stage_id = next
                .id
                .clone()
                .ok_or_else(|| SmError::Conflict("the next stage has no id".into()))?;
            Ok(OvenControl::StartStage { stage_id })
        }
        ControlRequest::Temperature { celsius } if in_preheat => {
            let mut stages = cook.raw_stages.clone();
            stages[active] = crate::protocol::stages::with_temperature(&stages[active], *celsius);
            Ok(update_cook_stages(&stages))
        }
        ControlRequest::Temperature { celsius } => Ok(OvenControl::SetTemperatureBulbs {
            mode: status.temperature_bulbs_mode.clone(),
            celsius: *celsius,
        }),
        ControlRequest::Timer { secs } if in_preheat => {
            if active + 1 >= cook.raw_stages.len() {
                return Err(SmError::Conflict(
                    "a preheat with no stage after it has no timer to set".into(),
                ));
            }
            let mut stages = cook.raw_stages.clone();
            stages[active + 1] = crate::protocol::stages::with_timer(&stages[active + 1], *secs);
            Ok(update_cook_stages(&stages))
        }
        ControlRequest::Timer { secs } => Ok(OvenControl::SetTimer { secs: *secs }),
        ControlRequest::Lamp { .. } | ControlRequest::TemperatureUnit { .. } => {
            unreachable!("handled above")
        }
    }
}

fn update_cook_stages(raw: &[serde_json::Value]) -> OvenControl {
    OvenControl::UpdateCookStages {
        stages: raw
            .iter()
            .map(crate::protocol::stages::for_update)
            .collect(),
    }
}

fn stop_cook(state: &mut SmState) -> (Result<(), SmError>, Vec<SmEffect>) {
    if matches!(state.lifecycle, Lifecycle::Idle) {
        return (Err(SmError::NotCooking), Vec::new());
    }
    let status_says_idle = state
        .status
        .as_ref()
        .map(|s| !s.is_cooking())
        .unwrap_or(false);
    if status_says_idle {
        return (Err(SmError::NotCooking), Vec::new());
    }
    if state.connectivity.cooker_id().is_none() {
        return (Err(SmError::Disconnected), Vec::new());
    }

    let request_id = Uuid::new_v4().to_string();
    state.pending.stop = Some(PendingStop {
        request_id: request_id.clone(),
    });
    state.set_lifecycle(Lifecycle::StopPending, TransitionOrigin::LocalCommand);

    (
        Ok(()),
        vec![SmEffect::Ws(WsCommand::SendStop { request_id })],
    )
}

// ─── External-event transitions ──────────────────────────────────────────────

fn reduce_ws_event(state: &mut SmState, event: WsEvent) -> Vec<SmEffect> {
    match event {
        WsEvent::Connected => {
            // Preserve existing cooker id if already known.
            let cooker = match &state.connectivity {
                Connectivity::Connected { cooker_id } => cooker_id.clone(),
                Connectivity::Disconnected => None,
            };
            state.connectivity = Connectivity::Connected { cooker_id: cooker };
            Vec::new()
        }
        WsEvent::Disconnected => {
            state.connectivity = Connectivity::Disconnected;
            Vec::new()
        }
        WsEvent::CookerDiscovered { cooker_id } => {
            state.connectivity = Connectivity::Connected {
                cooker_id: Some(cooker_id),
            };
            Vec::new()
        }
        WsEvent::ApoState { status, cook } => {
            // First, so a store that keeps history itself has recorded an
            // ended cook before any history refresh below. Not while a start
            // is pending: the oven's cook (or its idleness) is about to be
            // replaced, and would end the cook just started.
            let report = (state.lifecycle != Lifecycle::StartPending).then(|| {
                SmEffect::Store(StoreCommand::OvenReported {
                    oven_cook: cook.clone(),
                })
            });
            state.oven_cook = cook;
            let mut effects = apply_apo_state(state, status);
            if let Some(report) = report {
                effects.insert(0, report);
            }
            effects
        }
        WsEvent::CommandAck { request_id, status } => {
            apply_command_ack(state, &request_id, &status)
        }
        WsEvent::ParseError { detail } => {
            warn!(target: "sm", detail = %detail, "ws parse error");
            Vec::new()
        }
    }
}

fn apply_apo_state(state: &mut SmState, status: OvenStatus) -> Vec<SmEffect> {
    let prev_cooking = state
        .status
        .as_ref()
        .map(|s| s.is_cooking())
        .unwrap_or(false);
    let prev_timer = state.status.as_ref().map(|s| s.timer_total_secs);
    let seen_before = state.status.is_some();
    let is_cooking = status.is_cooking();
    let timer = status.timer_total_secs;

    let mut effects: Vec<SmEffect> = Vec::new();

    // Idle -> cooking (local start confirmed or external start).
    if is_cooking && !prev_cooking {
        let reason = if seen_before {
            CurrentCookReason::WsIdleToCooking
        } else {
            CurrentCookReason::WsConnectedCookingState
        };
        let origin = match state.lifecycle {
            Lifecycle::StartPending => TransitionOrigin::AckConfirmed,
            _ => {
                if seen_before {
                    TransitionOrigin::WsExternal
                } else {
                    TransitionOrigin::StartupSync
                }
            }
        };
        state.pending.start = None;
        state.set_lifecycle(Lifecycle::Cooking, origin);

        // After leaving StartPending: the oven was idle, so its cook is the
        // new one, not one being replaced.
        effects.push(fetch_current_cook(state, reason));
        effects.push(SmEffect::Store(StoreCommand::RefreshHistory {
            reason: HistoryReason::WsIdleToCooking,
        }));
    }

    // Cooking -> idle (local stop confirmed, external stop, or completion).
    if !is_cooking && prev_cooking {
        effects.push(SmEffect::Store(StoreCommand::RefreshHistory {
            reason: HistoryReason::WsCookToIdle,
        }));
        let origin = match state.lifecycle {
            Lifecycle::StopPending => TransitionOrigin::AckConfirmed,
            _ => TransitionOrigin::WsExternal,
        };
        state.pending.stop = None;
        state.current_cook = None;
        state.set_lifecycle(Lifecycle::Idle, origin);
    }

    // Stage transition while continuously cooking.
    if is_cooking && prev_cooking {
        if let Some(prev) = prev_timer {
            if prev != timer {
                effects.push(fetch_current_cook(
                    state,
                    CurrentCookReason::WsStageTransition,
                ));
            }
        }
    }

    // Reconcile any lingering StartPending when ApoState now says idle — the
    // start failed silently; roll back.
    if !is_cooking && matches!(state.lifecycle, Lifecycle::StartPending) {
        state.pending.start = None;
        state.set_lifecycle(Lifecycle::Idle, TransitionOrigin::TimeoutRecovery);
    }

    // Stale-cook reconcile: a settled-idle oven must not carry a current cook.
    // The cooking->idle edge above clears it, but that edge is only observed
    // if the server saw the prior cooking frame. Anova's Firestore currentCook
    // lingers after a cook ends, so if we never saw the edge (e.g. restart or
    // WS reconnect while already idle, or a silently-failed start rolled back
    // to Idle) a fetched-but-stale record would otherwise persist here and
    // drive phantom cook-progress.
    if !is_cooking && matches!(state.lifecycle, Lifecycle::Idle) {
        state.current_cook = None;
    }

    state.status = Some(status);
    effects.push(SmEffect::PublishReadModel);
    effects
}

fn apply_command_ack(state: &mut SmState, request_id: &str, status: &str) -> Vec<SmEffect> {
    let ok = status.eq_ignore_ascii_case("ok");

    // Start ack: on ok, trigger a current-cook refresh and wait for ApoState
    // to confirm the cook is active. The oven starts the first stage from the
    // CMD_APO_START payload; sending CMD_APO_START_STAGE for stage 0 causes a
    // protocol error on current firmware.
    if let Some(pending) = state.pending.start.as_ref() {
        if pending.request_id == request_id {
            if ok {
                state.last_origin = Some(TransitionOrigin::AckConfirmed);
                // In local mode the ack comes from the oven's own state, sent
                // just before it, so a start that replaced a running cook (no
                // idle -> cooking edge to resolve it) is confirmed here.
                let cooking = state.status.as_ref().is_some_and(|s| s.is_cooking());
                if state.local && cooking {
                    state.pending.start = None;
                    state.set_lifecycle(Lifecycle::Cooking, TransitionOrigin::AckConfirmed);
                }
                // Otherwise StartPending remains until ApoState confirms cooking.
                return vec![fetch_current_cook(
                    state,
                    CurrentCookReason::StartCommandAccepted,
                )];
            } else {
                state.pending.start = None;
                state.set_lifecycle(Lifecycle::Idle, TransitionOrigin::TimeoutRecovery);
                return vec![command_failed("start", status)];
            }
        }
    }

    if let Some(pending) = state.pending.stop.as_ref() {
        if pending.request_id == request_id && !ok {
            state.pending.stop = None;
            // On negative stop ack, fall back to Cooking and let ApoState
            // reconcile if the oven actually stopped.
            state.set_lifecycle(Lifecycle::Cooking, TransitionOrigin::TimeoutRecovery);
            return vec![command_failed("stop", status)];
        }
    }

    let controls = &mut state.pending.controls;
    if let Some(i) = controls.iter().position(|(id, _)| id == request_id) {
        let (_, name) = controls.remove(i);
        if !ok {
            // Nothing to roll back: the next state shows what the oven did.
            warn!(target: "sm", request_id, status, control = name, "oven control failed");
            return vec![command_failed(name, status)];
        }
    }
    Vec::new()
}

fn command_failed(command: &str, reason: &str) -> SmEffect {
    SmEffect::CommandFailed(CommandFailure {
        command: command.to_string(),
        reason: reason.to_string(),
    })
}

fn reduce_store_event(state: &mut SmState, event: StoreEvent) -> Vec<SmEffect> {
    match event {
        StoreEvent::RecipesRefreshed(Ok(recipes)) => {
            state.recipes = recipes;
            vec![SmEffect::PublishReadModel]
        }
        StoreEvent::RecipesRefreshed(Err(e)) => {
            warn!(target: "sm", error = %e, "recipes refresh failed");
            Vec::new()
        }
        StoreEvent::HistoryRefreshed(Ok(history)) => {
            state.history = history;
            vec![SmEffect::PublishReadModel]
        }
        StoreEvent::HistoryRefreshed(Err(e)) => {
            warn!(target: "sm", error = %e, "history refresh failed");
            Vec::new()
        }
        StoreEvent::CurrentCookFetched {
            result: Ok(cook), ..
        } => {
            state.current_cook = cook;
            vec![SmEffect::PublishReadModel]
        }
        StoreEvent::CurrentCookFetched {
            result: Err(StoreError::Unsupported),
            reason,
        } => {
            debug!(target: "sm", reason = ?reason, "store doesn't track the current cook; keeping ours");
            Vec::new()
        }
        StoreEvent::CurrentCookFetched {
            result: Err(e),
            reason,
        } => {
            warn!(target: "sm", reason = ?reason, error = %e, "current-cook fetch failed");
            Vec::new()
        }
        StoreEvent::RecipeRefPatched {
            result: Ok(()),
            cook_id,
            recipe_id,
        } => {
            info!(target: "sm", cook_id = %cook_id, recipe_id = ?recipe_id, "recipe ref patched");
            vec![fetch_current_cook(
                state,
                CurrentCookReason::StartCommandAccepted,
            )]
        }
        StoreEvent::RecipeRefPatched { result: Err(e), .. } => {
            warn!(target: "sm", error = %e, "recipe ref patch failed");
            Vec::new()
        }
        StoreEvent::ManualCookTitleResolved {
            result: Ok(Some((title, id))),
            cook_key,
        } => {
            if let Some(ref mut cook) = state.current_cook {
                // Heuristic: only update if the cook looks like the same one.
                if cook.started_at == cook_key && cook.recipe_title == "[manual]" {
                    cook.recipe_title = title;
                    if cook.recipe_id.is_none() {
                        cook.recipe_id = Some(id);
                    }
                }
            }
            vec![SmEffect::PublishReadModel]
        }
        StoreEvent::ManualCookTitleResolved { .. } => Vec::new(),
    }
}

// ─── Command outcome helper ──────────────────────────────────────────────────

/// A command reducer returns both the reply sender (with its typed result)
/// *and* any external effects. The outer loop dispatches these separately.
pub struct CommandOutcome {
    pub reply: ReplyChannel,
    pub effects: Vec<SmEffect>,
}

#[allow(clippy::large_enum_variant)]
pub enum ReplyChannel {
    Status(
        oneshot::Sender<Result<OvenStatus, SmError>>,
        Result<OvenStatus, SmError>,
    ),
    CurrentCook(
        oneshot::Sender<Result<Option<CurrentCook>, SmError>>,
        Result<Option<CurrentCook>, SmError>,
    ),
    Recipes(
        oneshot::Sender<Result<Vec<Recipe>, SmError>>,
        Result<Vec<Recipe>, SmError>,
    ),
    History(
        oneshot::Sender<Result<Vec<HistoryEntry>, SmError>>,
        Result<Vec<HistoryEntry>, SmError>,
    ),
    Unit(oneshot::Sender<Result<(), SmError>>, Result<(), SmError>),
}

impl CommandOutcome {
    fn reply_status(
        tx: oneshot::Sender<Result<OvenStatus, SmError>>,
        result: Result<OvenStatus, SmError>,
    ) -> Self {
        Self {
            reply: ReplyChannel::Status(tx, result),
            effects: Vec::new(),
        }
    }
    fn reply_current_cook(
        tx: oneshot::Sender<Result<Option<CurrentCook>, SmError>>,
        result: Result<Option<CurrentCook>, SmError>,
    ) -> Self {
        Self {
            reply: ReplyChannel::CurrentCook(tx, result),
            effects: Vec::new(),
        }
    }
    fn reply_recipes(
        tx: oneshot::Sender<Result<Vec<Recipe>, SmError>>,
        result: Result<Vec<Recipe>, SmError>,
        effects: Vec<SmEffect>,
    ) -> Self {
        Self {
            reply: ReplyChannel::Recipes(tx, result),
            effects,
        }
    }
    fn reply_history(
        tx: oneshot::Sender<Result<Vec<HistoryEntry>, SmError>>,
        result: Result<Vec<HistoryEntry>, SmError>,
    ) -> Self {
        Self {
            reply: ReplyChannel::History(tx, result),
            effects: Vec::new(),
        }
    }
    fn reply_unit_with_effects(
        tx: oneshot::Sender<Result<(), SmError>>,
        result: Result<(), SmError>,
        effects: Vec<SmEffect>,
    ) -> Self {
        Self {
            reply: ReplyChannel::Unit(tx, result),
            effects,
        }
    }

    pub fn deliver(self) -> Vec<SmEffect> {
        match self.reply {
            ReplyChannel::Status(tx, r) => {
                let _ = tx.send(r);
            }
            ReplyChannel::CurrentCook(tx, r) => {
                let _ = tx.send(r);
            }
            ReplyChannel::Recipes(tx, r) => {
                let _ = tx.send(r);
            }
            ReplyChannel::History(tx, r) => {
                let _ = tx.send(r);
            }
            ReplyChannel::Unit(tx, r) => {
                let _ = tx.send(r);
            }
        }
        self.effects
    }
}

// ─── Processor loop ──────────────────────────────────────────────────────────

pub struct StateMachineProcessor {
    pub cmd_rx: mpsc::Receiver<StateMachineCommand>,
    pub evt_rx: mpsc::Receiver<StateMachineEvent>,
    pub ws_tx: mpsc::Sender<WsCommand>,
    pub store_tx: mpsc::Sender<StoreCommand>,
    pub read_model_tx: watch::Sender<ReadModel>,
    /// Failed commands, for whoever listens (the MQTT processor). Sending
    /// with no listener is fine.
    pub failures_tx: broadcast::Sender<CommandFailure>,
    state: SmState,
}

impl StateMachineProcessor {
    pub fn new(
        cmd_rx: mpsc::Receiver<StateMachineCommand>,
        evt_rx: mpsc::Receiver<StateMachineEvent>,
        ws_tx: mpsc::Sender<WsCommand>,
        store_tx: mpsc::Sender<StoreCommand>,
        read_model_tx: watch::Sender<ReadModel>,
        failures_tx: broadcast::Sender<CommandFailure>,
        local: bool,
    ) -> Self {
        Self {
            cmd_rx,
            evt_rx,
            ws_tx,
            store_tx,
            read_model_tx,
            failures_tx,
            state: SmState {
                local,
                ..SmState::new()
            },
        }
    }

    pub async fn run(mut self) {
        info!(target: "sm", "state machine processor running");
        loop {
            tokio::select! {
                Some(cmd) = self.cmd_rx.recv() => {
                    let outcome = reduce_command(&mut self.state, cmd);
                    let effects = outcome.deliver();
                    self.dispatch(effects).await;
                }
                Some(evt) = self.evt_rx.recv() => {
                    let effects = reduce_event(&mut self.state, evt);
                    self.dispatch(effects).await;
                }
                else => break,
            }
        }
    }

    async fn dispatch(&self, effects: Vec<SmEffect>) {
        for effect in effects {
            match effect {
                SmEffect::Ws(cmd) => {
                    if let Err(e) = self.ws_tx.send(cmd).await {
                        warn!(target: "sm", error = %e, "ws channel closed");
                    }
                }
                SmEffect::Store(cmd) => {
                    if let Err(e) = self.store_tx.send(cmd).await {
                        warn!(target: "sm", error = %e, "store channel closed");
                    }
                }
                SmEffect::PublishReadModel => {
                    let _ = self.read_model_tx.send(self.state.to_read_model());
                }
                SmEffect::CommandFailed(failure) => {
                    let _ = self.failures_tx.send(failure);
                }
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use anova_oven_api::Stage;

    fn oven_status(mode: &str, timer_total_secs: u64) -> OvenStatus {
        OvenStatus {
            mode: mode.into(),
            temperature_unit: "F".into(),
            temperature_c: 25.0,
            target_temperature_c: None,
            temperature_bulbs_mode: "dry".into(),
            dry_top_temperature_c: 25.0,
            dry_bottom_temperature_c: 25.0,
            wet_bulb_temperature_c: 25.0,
            probe_temperature_c: None,
            probe_target_temperature_c: None,
            wet_target_temperature_c: None,
            timer_current_secs: 0,
            timer_total_secs,
            timer_mode: if timer_total_secs > 0 {
                "running".into()
            } else {
                "idle".into()
            },
            steam_pct: 0.0,
            steam_target_pct: None,
            steam_generator_mode: "idle".into(),
            boiler_celsius: 25.0,
            boiler_watts: 0.0,
            boiler_descale_required: false,
            evaporator_celsius: 25.0,
            evaporator_watts: 0.0,
            fan_speed: 0,
            heating_element_top_on: false,
            heating_element_top_watts: 0.0,
            heating_element_rear_on: false,
            heating_element_rear_watts: 0.0,
            heating_element_bottom_on: false,
            heating_element_bottom_watts: 0.0,
            lamp_on: false,
            lamp_preference: "on".into(),
            vent_open: false,
            door_open: false,
            water_tank_empty: false,
            active_stage_index: None,
            active_stage_id: None,
            cook_done: false,
            cook_id: None,
            stage_transition_pending_user_action: false,
            cook_progress: None,
            upstream: None,
        }
    }

    fn stage(
        id: Option<&str>,
        kind: &str,
        user_action_required: bool,
        duration: Option<u64>,
    ) -> Stage {
        Stage {
            id: id.map(Into::into),
            kind: kind.into(),
            temperature_c: 180.0,
            temperature_bulbs_mode: Some("dry".into()),
            duration_secs: duration,
            timer_added: Some(duration.is_some()),
            probe_added: Some(false),
            probe_target_c: None,
            steam_pct: 0.0,
            fan_speed: 75,
            user_action_required: Some(user_action_required),
            rack_position: Some(3),
            heating_element_top: Some(true),
            heating_element_rear: Some(true),
            heating_element_bottom: Some(true),
            vent_open: Some(false),
            title: None,
        }
    }

    fn cook(stages: Vec<Stage>) -> CurrentCook {
        let total = stages.len();
        CurrentCook {
            recipe_id: None,
            recipe_title: "[manual]".into(),
            started_at: "2026-04-17T00:00:00Z".into(),
            cook_stage_count: total,
            total_stage_count: total,
            stages,
        }
    }

    fn effect_ws(effects: &[SmEffect]) -> Option<&WsCommand> {
        effects.iter().find_map(|e| match e {
            SmEffect::Ws(c) => Some(c),
            _ => None,
        })
    }

    fn store_cmds(effects: &[SmEffect]) -> Vec<&StoreCommand> {
        effects
            .iter()
            .filter_map(|e| match e {
                SmEffect::Store(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    // 1. idle -> cooking triggers current-cook + history refresh effects.
    #[test]
    fn idle_to_cooking_triggers_refresh_effects() {
        let mut state = SmState::new();
        // Seed a prior idle status so `seen_before=true` path is taken.
        let _ = apply_apo_state(&mut state, oven_status("idle", 0));
        let effects = apply_apo_state(&mut state, oven_status("cook", 1200));

        let fs = store_cmds(&effects);
        assert!(fs.iter().any(|c| matches!(
            c,
            StoreCommand::FetchCurrentCook {
                reason: CurrentCookReason::WsIdleToCooking,
                ..
            }
        )));
        assert!(fs.iter().any(|c| matches!(
            c,
            StoreCommand::RefreshHistory {
                reason: HistoryReason::WsIdleToCooking
            }
        )));
        assert_eq!(state.lifecycle, Lifecycle::Cooking);
    }

    // 2. cooking -> idle triggers history refresh effect + clears cook state.
    #[test]
    fn cooking_to_idle_triggers_history_refresh() {
        let mut state = SmState::new();
        state.current_cook = Some(cook(vec![stage(Some("s1"), "cook", false, Some(1200))]));
        let _ = apply_apo_state(&mut state, oven_status("cook", 1200));
        let effects = apply_apo_state(&mut state, oven_status("idle", 0));

        let fs = store_cmds(&effects);
        assert!(fs.iter().any(|c| matches!(
            c,
            StoreCommand::RefreshHistory {
                reason: HistoryReason::WsCookToIdle
            }
        )));
        assert_eq!(state.lifecycle, Lifecycle::Idle);
        assert!(state.current_cook.is_none());
    }

    // Stale current_cook is cleared on an idle ApoState even when the
    // cooking->idle edge was never observed (e.g. server restart / WS
    // reconnect while already idle) — the stale-cook reconcile.
    #[test]
    fn idle_apo_state_clears_stale_current_cook_without_edge() {
        let mut state = SmState::new();
        // Populated out-of-band, with no prior cooking status observed, so the
        // cooking->idle edge below will not fire — only the reconcile can.
        state.current_cook = Some(cook(vec![stage(Some("s1"), "cook", false, Some(1200))]));

        let _ = apply_apo_state(&mut state, oven_status("idle", 0));

        assert_eq!(state.lifecycle, Lifecycle::Idle);
        assert!(state.current_cook.is_none());
    }

    // 3. stop when idle -> NotCooking
    #[test]
    fn stop_rejects_when_idle() {
        let mut state = SmState::new();
        let (result, effects) = stop_cook(&mut state);
        assert!(matches!(result, Err(SmError::NotCooking)));
        assert!(effects.is_empty());
    }

    // Start-pending to cooking on ApoState cooking.
    #[test]
    fn start_pending_resolves_on_apo_state_cooking() {
        let mut state = SmState::new();
        state.lifecycle = Lifecycle::StartPending;
        state.pending.start = Some(PendingStart {
            request_id: "r1".into(),
            first_stage_id: Some("s1".into()),
        });
        let _ = apply_apo_state(&mut state, oven_status("cook", 1200));
        assert_eq!(state.lifecycle, Lifecycle::Cooking);
        assert!(state.pending.start.is_none());
        assert!(matches!(
            state.last_origin,
            Some(TransitionOrigin::AckConfirmed)
        ));
    }

    // Start-pending ack with status=ok only triggers a current-cook refresh.
    #[test]
    fn start_ack_ok_triggers_current_cook_refresh() {
        let mut state = SmState::new();
        state.lifecycle = Lifecycle::StartPending;
        state.pending.start = Some(PendingStart {
            request_id: "r1".into(),
            first_stage_id: Some("s1".into()),
        });

        let effects = apply_command_ack(&mut state, "r1", "ok");
        assert!(effects.iter().any(|effect| matches!(
            effect,
            SmEffect::Store(StoreCommand::FetchCurrentCook {
                reason: CurrentCookReason::StartCommandAccepted,
                ..
            })
        )));
        // Lifecycle remains StartPending until ApoState says cooking.
        assert_eq!(state.lifecycle, Lifecycle::StartPending);
        // Pending tracker preserved so ApoState can match.
        assert!(state.pending.start.is_some());
    }

    // Non-matching request id ack is a no-op.
    #[test]
    fn command_ack_with_unknown_request_id_is_noop() {
        let mut state = SmState::new();
        state.lifecycle = Lifecycle::StartPending;
        state.pending.start = Some(PendingStart {
            request_id: "r1".into(),
            first_stage_id: Some("s1".into()),
        });

        let effects = apply_command_ack(&mut state, "other", "ok");
        assert!(effects.is_empty());
        assert_eq!(state.lifecycle, Lifecycle::StartPending);
        assert!(state.pending.start.is_some());
    }

    // Start-pending ack with error clears pending and rolls back to Idle.
    #[test]
    fn start_ack_error_rolls_back_to_idle() {
        let mut state = SmState::new();
        state.lifecycle = Lifecycle::StartPending;
        state.pending.start = Some(PendingStart {
            request_id: "r1".into(),
            first_stage_id: Some("s1".into()),
        });

        let effects = apply_command_ack(&mut state, "r1", "error");
        assert_eq!(
            failures(&effects),
            [("start".to_string(), "error".to_string())]
        );
        assert_eq!(state.lifecycle, Lifecycle::Idle);
        assert!(state.pending.start.is_none());
        assert!(matches!(
            state.last_origin,
            Some(TransitionOrigin::TimeoutRecovery)
        ));
    }

    fn recipe(stages: Vec<Stage>) -> Recipe {
        Recipe {
            id: "r".into(),
            title: "Recipe".into(),
            stage_count: stages.len(),
            stages,
            source: Default::default(),
        }
    }

    fn local_state() -> SmState {
        SmState {
            local: true,
            connectivity: Connectivity::Connected {
                cooker_id: Some("oven".into()),
            },
            ..SmState::new()
        }
    }

    #[test]
    fn local_start_ack_confirms_a_start_over_a_running_cook() {
        let mut state = local_state();
        let _ = apply_apo_state(&mut state, oven_status("cook", 600));
        state.recipes = vec![recipe(vec![stage(Some("s1"), "cook", false, Some(600))])];
        let (_, effects) = start_cook(&mut state, "r");
        let Some(WsCommand::SendStart { request_id, .. }) = effect_ws(&effects) else {
            panic!("expected a start");
        };
        let request_id = request_id.clone();
        assert_eq!(state.lifecycle, Lifecycle::StartPending);

        let _ = apply_command_ack(&mut state, &request_id, "ok");
        assert_eq!(state.lifecycle, Lifecycle::Cooking);
        assert!(state.pending.start.is_none());
    }

    #[test]
    fn a_manual_cook_starts_without_a_recipe_and_journals_its_title() {
        let mut state = local_state();
        let _ = apply_apo_state(&mut state, oven_status("idle", 0));
        let stages = vec![
            stage(Some("s1-preheat"), "preheat", false, None),
            stage(Some("s1"), "cook", false, Some(600)),
        ];
        let (result, effects) = start_manual_cook(&mut state, "Quick roast".into(), stages);
        assert!(result.is_ok());
        let Some(WsCommand::SendStart { stages, .. }) = effect_ws(&effects) else {
            panic!("expected a start");
        };
        assert_eq!(stages.len(), 2);
        let from = effects.iter().find_map(|e| match e {
            SmEffect::Store(StoreCommand::PatchCookRecipeRef { from, .. }) => Some(from.clone()),
            _ => None,
        });
        assert_eq!(
            from,
            Some(StartedFrom::Manual {
                title: "Quick roast".into(),
                cook_stage_count: 1
            })
        );
        let cook = state.current_cook.as_ref().unwrap();
        assert_eq!(cook.recipe_title, "Quick roast");
        assert_eq!(cook.recipe_id, None);
        assert_eq!(state.lifecycle, Lifecycle::StartPending);
    }

    fn oven_cook(cook_id: &str) -> OvenCook {
        OvenCook {
            cook_id: cook_id.into(),
            stages: vec![stage(Some("s1"), "cook", false, Some(600))],
            raw_stages: Vec::new(),
            seconds_elapsed: 0,
        }
    }

    /// A local-mode state cooking `stages` (kinds) on `active`, with the
    /// oven's raw stage JSON to match.
    fn cooking_on(kinds: &[&str], active: usize) -> SmState {
        let raw: Vec<serde_json::Value> = kinds
            .iter()
            .enumerate()
            .map(|(i, kind)| {
                serde_json::json!({
                    "id": format!("stage-{i}"),
                    "type": kind,
                    "title": format!("{kind} {i}"),
                    "userActionRequired": false,
                    "temperatureBulbs": { "mode": "dry", "dry": { "setpoint": { "celsius": 200, "fahrenheit": 392 } } },
                    "timer": { "initial": 600 },
                    "rackPosition": 2,
                })
            })
            .collect();
        let mut state = local_state();
        let mut status = oven_status("cook", 600);
        status.active_stage_index = Some(active);
        let _ = reduce_ws_event(
            &mut state,
            WsEvent::ApoState {
                status,
                cook: Some(OvenCook {
                    cook_id: "c".into(),
                    stages: raw
                        .iter()
                        .map(crate::protocol::stages::stage_from_json)
                        .collect(),
                    raw_stages: raw,
                    seconds_elapsed: 0,
                }),
            },
        );
        state
    }

    fn failures(effects: &[SmEffect]) -> Vec<(String, String)> {
        effects
            .iter()
            .filter_map(|e| match e {
                SmEffect::CommandFailed(f) => Some((f.command.clone(), f.reason.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn failed_acks_are_announced_by_command() {
        let mut state = cooking_on(&["cook", "cook"], 0);
        let outcome = reduce_command(
            &mut state,
            StateMachineCommand::Control {
                request: ControlRequest::NextStage,
                reply: oneshot::channel().0,
            },
        );
        let effects = outcome.deliver();
        let Some(WsCommand::Control { request_id, .. }) = effect_ws(&effects) else {
            panic!("expected a control");
        };
        let request_id = request_id.clone();
        assert_eq!(
            failures(&apply_command_ack(&mut state, &request_id, "timeout")),
            [("next_stage".to_string(), "timeout".to_string())]
        );
        // Once only.
        assert!(failures(&apply_command_ack(&mut state, &request_id, "timeout")).is_empty());

        let (_, effects) = stop_cook(&mut state);
        let Some(WsCommand::SendStop { request_id }) = effect_ws(&effects) else {
            panic!("expected a stop");
        };
        let request_id = request_id.clone();
        assert_eq!(
            failures(&apply_command_ack(&mut state, &request_id, "disconnected")),
            [("stop".to_string(), "disconnected".to_string())]
        );
    }

    #[test]
    fn controls_are_refused_in_cloud_mode() {
        let mut state = cooking_on(&["cook"], 0);
        state.local = false;
        assert!(matches!(
            control(&state, &ControlRequest::Lamp { on: true }),
            Err(SmError::Unsupported(_))
        ));
    }

    #[test]
    fn next_stage_starts_the_following_stage_by_id() {
        let state = cooking_on(&["preheat", "cook"], 0);
        assert!(matches!(
            control(&state, &ControlRequest::NextStage),
            Ok(OvenControl::StartStage { stage_id }) if stage_id == "stage-1"
        ));
        let last = cooking_on(&["preheat", "cook"], 1);
        assert!(matches!(
            control(&last, &ControlRequest::NextStage),
            Err(SmError::Conflict(_))
        ));
    }

    #[test]
    fn stage_controls_need_a_cook() {
        let mut state = local_state();
        let _ = apply_apo_state(&mut state, oven_status("idle", 0));
        assert!(matches!(
            control(&state, &ControlRequest::Timer { secs: 60 }),
            Err(SmError::NotCooking)
        ));
        // The lamp and unit don't.
        assert!(control(&state, &ControlRequest::Lamp { on: true }).is_ok());
    }

    #[test]
    fn cook_stage_edits_use_set_commands() {
        let state = cooking_on(&["preheat", "cook"], 1);
        assert!(matches!(
            control(&state, &ControlRequest::Timer { secs: 60 }),
            Ok(OvenControl::SetTimer { secs: 60 })
        ));
        assert!(matches!(
            control(&state, &ControlRequest::Temperature { celsius: 180.0 }),
            Ok(OvenControl::SetTemperatureBulbs { mode, celsius }) if mode == "dry" && celsius == 180.0
        ));
    }

    #[test]
    fn preheat_edits_go_out_as_a_stage_update() {
        let state = cooking_on(&["preheat", "cook"], 0);

        let Ok(OvenControl::UpdateCookStages { stages }) =
            control(&state, &ControlRequest::Temperature { celsius: 180.0 })
        else {
            panic!("expected an update");
        };
        // The preheat's own setpoint; the cook stage keeps its own.
        assert_eq!(
            stages[0]["temperatureBulbs"]["dry"]["setpoint"]["celsius"],
            180
        );
        assert_eq!(
            stages[1]["temperatureBulbs"]["dry"]["setpoint"]["celsius"],
            200
        );
        // Sent back as the oven had them, plus what the update format needs.
        assert_eq!(stages[0]["id"], "stage-0");
        assert_eq!(stages[0]["rackPosition"], 2);
        assert_eq!(stages[0]["stepType"], "stage");
        assert_eq!(stages[0]["stageTransitionType"], "automatic");

        let Ok(OvenControl::UpdateCookStages { stages }) =
            control(&state, &ControlRequest::Timer { secs: 90 })
        else {
            panic!("expected an update");
        };
        // The timer the oven shows during a preheat is the next stage's.
        assert_eq!(stages[1]["timer"]["initial"], 90);
        assert_eq!(stages[0]["timer"]["initial"], 600);
    }

    fn fetched_oven_cook_ids(effects: &[SmEffect]) -> Vec<Option<String>> {
        store_cmds(effects)
            .into_iter()
            .filter_map(|c| match c {
                StoreCommand::FetchCurrentCook { oven_cook, .. } => {
                    Some(oven_cook.as_ref().map(|c| c.cook_id.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn cooking(state: &mut SmState, cook_id: &str) -> Vec<SmEffect> {
        reduce_ws_event(
            state,
            WsEvent::ApoState {
                status: oven_status("cook", 600),
                cook: Some(oven_cook(cook_id)),
            },
        )
    }

    #[test]
    fn idle_to_cooking_fetches_with_the_new_oven_cook() {
        let mut state = local_state();
        let _ = apply_apo_state(&mut state, oven_status("idle", 0));
        state.recipes = vec![recipe(vec![stage(Some("s1"), "cook", false, Some(600))])];
        let _ = start_cook(&mut state, "r");
        let effects = cooking(&mut state, "ios-new");
        assert_eq!(
            fetched_oven_cook_ids(&effects),
            [Some("ios-new".to_string())]
        );
    }

    #[test]
    fn a_start_over_a_running_cook_never_fetches_with_the_old_one() {
        let mut state = local_state();
        let _ = cooking(&mut state, "old");
        state.recipes = vec![recipe(vec![stage(Some("s1"), "cook", false, Some(600))])];
        let (_, effects) = start_cook(&mut state, "r");
        let Some(WsCommand::SendStart { request_id, .. }) = effect_ws(&effects) else {
            panic!("expected a start");
        };
        let request_id = request_id.clone();

        // The periodic refresh while the start is pending.
        let tick = reduce_event(
            &mut state,
            StateMachineEvent::Tick(crate::runtime::types::TickKind::CurrentCookRefresh),
        );
        assert_eq!(fetched_oven_cook_ids(&tick), [None]);

        // The oven reports the new cook, then the ack.
        let _ = cooking(&mut state, "ios-new");
        let effects = apply_command_ack(&mut state, &request_id, "ok");
        assert_eq!(
            fetched_oven_cook_ids(&effects),
            [Some("ios-new".to_string())]
        );
    }

    fn reports(effects: &[SmEffect]) -> Vec<Option<String>> {
        store_cmds(effects)
            .into_iter()
            .filter_map(|c| match c {
                StoreCommand::OvenReported { oven_cook } => {
                    Some(oven_cook.as_ref().map(|c| c.cook_id.clone()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn oven_report_goes_to_the_store_before_the_history_refresh() {
        let mut state = SmState::new();
        let _ = cooking(&mut state, "c1");
        let effects = reduce_ws_event(
            &mut state,
            WsEvent::ApoState {
                status: oven_status("idle", 0),
                cook: None,
            },
        );
        let cmds = store_cmds(&effects);
        assert!(matches!(
            cmds.first(),
            Some(StoreCommand::OvenReported { oven_cook: None })
        ));
        assert!(cmds
            .iter()
            .any(|c| matches!(c, StoreCommand::RefreshHistory { .. })));
    }

    #[test]
    fn oven_reports_are_held_back_while_a_start_is_pending() {
        let mut state = local_state();
        let _ = apply_apo_state(&mut state, oven_status("idle", 0));
        state.recipes = vec![recipe(vec![stage(Some("s1"), "cook", false, Some(600))])];
        let _ = start_cook(&mut state, "r");
        // An idle report that crossed the start on its way.
        let idle = reduce_ws_event(
            &mut state,
            WsEvent::ApoState {
                status: oven_status("idle", 0),
                cook: None,
            },
        );
        assert!(reports(&idle).is_empty());
        // Once the oven reports the new cook, reports resume.
        assert_eq!(
            reports(&cooking(&mut state, "ios-new")),
            [Some("ios-new".to_string())]
        );
    }

    #[test]
    fn unsupported_current_cook_keeps_ours() {
        let mut state = SmState::new();
        state.current_cook = Some(cook(vec![stage(Some("s1"), "cook", false, Some(600))]));
        let _ = reduce_store_event(
            &mut state,
            StoreEvent::CurrentCookFetched {
                reason: CurrentCookReason::Periodic,
                result: Err(StoreError::Unsupported),
            },
        );
        assert!(state.current_cook.is_some());
    }

    // External cooking transition (no matching StartPending) uses WsExternal origin.
    #[test]
    fn external_cooking_transition_origin_is_ws_external() {
        let mut state = SmState::new();
        let _ = apply_apo_state(&mut state, oven_status("idle", 0));
        let _ = apply_apo_state(&mut state, oven_status("cook", 1200));
        assert!(matches!(
            state.last_origin,
            Some(TransitionOrigin::WsExternal)
        ));
    }
}
