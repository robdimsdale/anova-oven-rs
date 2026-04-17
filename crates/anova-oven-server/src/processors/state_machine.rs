//! State machine processor.
//!
//! Owns canonical oven/app state and orchestrates WebSocket + Firestore
//! effects via pure reducers. Phase 4 builds out the transition logic, pending
//! command tracker, and read-model snapshot. The processor is *built* here
//! but not yet wired into `main.rs` — Phase 5 will swap HTTP handlers onto it.
//!
//! Side effects are returned as `SmEffect` values and dispatched by the outer
//! event loop, keeping [`reduce_command`] / [`reduce_event`] free of IO and
//! directly unit-testable.

#![allow(dead_code)]

use anova_oven_api::{CookProgress, CurrentCook, HistoryEntry, OvenStatus, Recipe};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::read_model::ReadModel;
use crate::runtime::types::{
    CurrentCookReason, FirestoreCommand, FirestoreEvent, HistoryReason, SmError,
    StateMachineCommand, StateMachineEvent, TransitionOrigin, WsCommand, WsEvent,
};

// ─── Canonical state ─────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct SmState {
    pub connectivity: Connectivity,
    pub lifecycle: Lifecycle,
    pub last_origin: Option<TransitionOrigin>,
    pub pending: PendingTracker,
    pub status: Option<OvenStatus>,
    pub current_cook: Option<CurrentCook>,
    pub cook_progress: Option<CookProgress>,
    pub recipes: Vec<Recipe>,
    pub history: Vec<HistoryEntry>,
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
}

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
    Firestore(FirestoreCommand),
    PublishReadModel,
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
            let effects = vec![SmEffect::Firestore(FirestoreCommand::RefreshRecipes)];
            CommandOutcome::reply_recipes(reply, Ok(state.recipes.clone()), effects)
        }
        StateMachineCommand::StartCook { recipe_id, reply } => {
            let (result, effects) = start_cook(state, &recipe_id);
            CommandOutcome::reply_unit_with_effects(reply, result, effects)
        }
        StateMachineCommand::StopCook { reply } => {
            let (result, effects) = stop_cook(state);
            CommandOutcome::reply_unit_with_effects(reply, result, effects)
        }
    }
}

/// Reduce an external event into state mutation + effects. No replies here —
/// events originate from the WS / Firestore processors, not client calls.
pub fn reduce_event(state: &mut SmState, evt: StateMachineEvent) -> Vec<SmEffect> {
    match evt {
        StateMachineEvent::Ws(event) => reduce_ws_event(state, event),
        StateMachineEvent::Firestore(event) => reduce_firestore_event(state, event),
        StateMachineEvent::Tick(kind) => match kind {
            crate::runtime::types::TickKind::CurrentCookRefresh => {
                if matches!(
                    state.lifecycle,
                    Lifecycle::Cooking | Lifecycle::StartPending
                ) {
                    vec![SmEffect::Firestore(FirestoreCommand::FetchCurrentCook {
                        reason: CurrentCookReason::Periodic,
                    })]
                } else {
                    Vec::new()
                }
            }
            crate::runtime::types::TickKind::RecipesRefresh => {
                vec![SmEffect::Firestore(FirestoreCommand::RefreshRecipes)]
            }
            crate::runtime::types::TickKind::HistoryRefresh => {
                vec![SmEffect::Firestore(FirestoreCommand::RefreshHistory {
                    reason: HistoryReason::Periodic,
                })]
            }
            crate::runtime::types::TickKind::PendingTimeout => Vec::new(),
        },
    }
}

// ─── Client-command transitions ──────────────────────────────────────────────

fn start_cook(state: &mut SmState, recipe_id: &str) -> (Result<(), SmError>, Vec<SmEffect>) {
    // Guardrails: connected + known cooker id + recipe resolvable.
    if state.connectivity.cooker_id().is_none() {
        return (Err(SmError::Disconnected), Vec::new());
    }

    let mut recipe = match state.recipes.iter().find(|r| r.id == recipe_id).cloned() {
        Some(r) => r,
        None => return (Err(SmError::RecipeNotFound(recipe_id.into())), Vec::new()),
    };
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
        recipe_id: Some(recipe.id.clone()),
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
        SmEffect::Firestore(FirestoreCommand::PatchCookRecipeRef {
            cook_id,
            recipe_id: recipe.id.clone(),
        }),
    ];

    (Ok(()), effects)
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
        WsEvent::ApoState(status) => apply_apo_state(state, status),
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
        effects.push(SmEffect::Firestore(FirestoreCommand::FetchCurrentCook {
            reason,
        }));
        effects.push(SmEffect::Firestore(FirestoreCommand::RefreshHistory {
            reason: HistoryReason::WsIdleToCooking,
        }));

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
    }

    // Cooking -> idle (local stop confirmed, external stop, or completion).
    if !is_cooking && prev_cooking {
        effects.push(SmEffect::Firestore(FirestoreCommand::RefreshHistory {
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
                effects.push(SmEffect::Firestore(FirestoreCommand::FetchCurrentCook {
                    reason: CurrentCookReason::WsStageTransition,
                }));
            }
        }
    }

    // Reconcile any lingering StartPending when ApoState now says idle — the
    // start failed silently; roll back.
    if !is_cooking && matches!(state.lifecycle, Lifecycle::StartPending) {
        state.pending.start = None;
        state.set_lifecycle(Lifecycle::Idle, TransitionOrigin::TimeoutRecovery);
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
                let out = vec![SmEffect::Firestore(FirestoreCommand::FetchCurrentCook {
                    reason: CurrentCookReason::StartCommandAccepted,
                })];
                state.last_origin = Some(TransitionOrigin::AckConfirmed);
                // StartPending remains until ApoState confirms cooking.
                return out;
            } else {
                state.pending.start = None;
                state.set_lifecycle(Lifecycle::Idle, TransitionOrigin::TimeoutRecovery);
                return Vec::new();
            }
        }
    }

    if let Some(pending) = state.pending.stop.as_ref() {
        if pending.request_id == request_id && !ok {
            state.pending.stop = None;
            // On negative stop ack, fall back to Cooking and let ApoState
            // reconcile if the oven actually stopped.
            state.set_lifecycle(Lifecycle::Cooking, TransitionOrigin::TimeoutRecovery);
            return Vec::new();
        }
    }

    Vec::new()
}

fn reduce_firestore_event(state: &mut SmState, event: FirestoreEvent) -> Vec<SmEffect> {
    match event {
        FirestoreEvent::RecipesRefreshed(Ok(recipes)) => {
            state.recipes = recipes;
            vec![SmEffect::PublishReadModel]
        }
        FirestoreEvent::RecipesRefreshed(Err(e)) => {
            warn!(target: "sm", error = %e, "recipes refresh failed");
            Vec::new()
        }
        FirestoreEvent::HistoryRefreshed(Ok(history)) => {
            state.history = history;
            vec![SmEffect::PublishReadModel]
        }
        FirestoreEvent::HistoryRefreshed(Err(e)) => {
            warn!(target: "sm", error = %e, "history refresh failed");
            Vec::new()
        }
        FirestoreEvent::CurrentCookFetched {
            result: Ok(cook), ..
        } => {
            state.current_cook = cook;
            vec![SmEffect::PublishReadModel]
        }
        FirestoreEvent::CurrentCookFetched {
            result: Err(e),
            reason,
        } => {
            warn!(target: "sm", reason = ?reason, error = %e, "current-cook fetch failed");
            Vec::new()
        }
        FirestoreEvent::RecipeRefPatched {
            result: Ok(()),
            cook_id,
            recipe_id,
        } => {
            info!(target: "sm", cook_id = %cook_id, recipe_id = %recipe_id, "recipe ref patched");
            vec![SmEffect::Firestore(FirestoreCommand::FetchCurrentCook {
                reason: CurrentCookReason::StartCommandAccepted,
            })]
        }
        FirestoreEvent::RecipeRefPatched { result: Err(e), .. } => {
            warn!(target: "sm", error = %e, "recipe ref patch failed");
            Vec::new()
        }
        FirestoreEvent::ManualCookTitleResolved {
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
        FirestoreEvent::ManualCookTitleResolved { .. } => Vec::new(),
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
    pub fs_tx: mpsc::Sender<FirestoreCommand>,
    pub read_model_tx: watch::Sender<ReadModel>,
    state: SmState,
}

impl StateMachineProcessor {
    pub fn new(
        cmd_rx: mpsc::Receiver<StateMachineCommand>,
        evt_rx: mpsc::Receiver<StateMachineEvent>,
        ws_tx: mpsc::Sender<WsCommand>,
        fs_tx: mpsc::Sender<FirestoreCommand>,
        read_model_tx: watch::Sender<ReadModel>,
    ) -> Self {
        Self {
            cmd_rx,
            evt_rx,
            ws_tx,
            fs_tx,
            read_model_tx,
            state: SmState::new(),
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
                SmEffect::Firestore(cmd) => {
                    if let Err(e) = self.fs_tx.send(cmd).await {
                        warn!(target: "sm", error = %e, "firestore channel closed");
                    }
                }
                SmEffect::PublishReadModel => {
                    let _ = self.read_model_tx.send(self.state.to_read_model());
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
            cook_progress: None,
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

    fn effect_ws<'a>(effects: &'a [SmEffect]) -> Option<&'a WsCommand> {
        effects.iter().find_map(|e| match e {
            SmEffect::Ws(c) => Some(c),
            _ => None,
        })
    }

    fn firestore_cmds(effects: &[SmEffect]) -> Vec<&FirestoreCommand> {
        effects
            .iter()
            .filter_map(|e| match e {
                SmEffect::Firestore(c) => Some(c),
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

        let fs = firestore_cmds(&effects);
        assert!(fs.iter().any(|c| matches!(
            c,
            FirestoreCommand::FetchCurrentCook {
                reason: CurrentCookReason::WsIdleToCooking
            }
        )));
        assert!(fs.iter().any(|c| matches!(
            c,
            FirestoreCommand::RefreshHistory {
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

        let fs = firestore_cmds(&effects);
        assert!(fs.iter().any(|c| matches!(
            c,
            FirestoreCommand::RefreshHistory {
                reason: HistoryReason::WsCookToIdle
            }
        )));
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
            SmEffect::Firestore(FirestoreCommand::FetchCurrentCook {
                reason: CurrentCookReason::StartCommandAccepted,
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
        assert!(effects.is_empty());
        assert_eq!(state.lifecycle, Lifecycle::Idle);
        assert!(state.pending.start.is_none());
        assert!(matches!(
            state.last_origin,
            Some(TransitionOrigin::TimeoutRecovery)
        ));
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
