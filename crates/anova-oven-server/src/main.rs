//! Local HTTP server for the Anova Precision Oven.
//!
//! Maintains a persistent WebSocket connection to the Anova cloud API and a
//! Firestore connection for recipes and cook history. Exposes a simplified
//! plain-HTTP JSON API consumed by `anova-oven-cli` and `anova-oven-pico`.
//!
//! # Credentials (required env vars)
//! - `ANOVA_TOKEN`    — PAT token for the Anova WebSocket API
//! - `ANOVA_EMAIL`    — Firebase email
//! - `ANOVA_PASSWORD` — Firebase password
//!
//! # Endpoints
//! - `GET /status`       — current oven state
//! - `GET /recipes`      — user's saved recipes
//! - `GET /history`      — cook history (last 50 cooks)
//! - `GET /current-cook` — in-progress cook (diagnostic)
//! - `POST /update-recipes` — refresh recipe cache from Firestore
//! - `POST /stop`        — stop the current cook
//! - `POST /start`       — start a cook with a recipe (recipe ID in JSON body)

mod firestore;
mod protocol;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderName, HeaderValue, Uri};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_websockets::{ClientBuilder, Message};
use tracing::{debug, info, trace, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use uuid::Uuid;

const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 10;
const DEFAULT_HTTP_CONNECT_TIMEOUT_SECS: u64 = 5;
const DEFAULT_CURRENT_COOK_TIMEOUT_SECS: u64 = 4;
const DEFAULT_CURRENT_COOK_RESOLUTION_TIMEOUT_SECS: u64 = 1;
const DEFAULT_CURRENT_COOK_REFRESH_INTERVAL_SECS: u64 = 60;
const DEFAULT_RECIPES_REFRESH_INTERVAL_SECS: u64 = 3600;
const DEFAULT_RECIPES_REFRESH_TIMEOUT_SECS: u64 = 15;
const DEFAULT_HISTORY_REFRESH_INTERVAL_SECS: u64 = 3600;
const DEFAULT_HISTORY_REFRESH_TIMEOUT_SECS: u64 = 15;
const DEFAULT_HISTORY_ON_TRANSITION_MAX_WAIT_SECS: u64 = 10;
const DEFAULT_HISTORY_ON_TRANSITION_POLL_INTERVAL_SECS: u64 = 2;

// ─── Shared application state ─────────────────────────────────────────────────

enum WsCommand {
    Stop,
    Start {
        recipe_id: String,
        stages: Vec<anova_oven_api::Stage>,
    },
}

#[derive(Clone, Copy, Debug)]
enum CurrentCookRefreshReason {
    Periodic,
    Startup,
    WsIdleToCooking,
    WsConnectedCookingState,
    WsStageTransition,
    StartCommandAccepted,
}

impl CurrentCookRefreshReason {
    fn as_str(self) -> &'static str {
        match self {
            CurrentCookRefreshReason::Periodic => "periodic",
            CurrentCookRefreshReason::Startup => "startup",
            CurrentCookRefreshReason::WsIdleToCooking => "ws-idle-to-cooking",
            CurrentCookRefreshReason::WsConnectedCookingState => "ws-connected-cooking-state",
            CurrentCookRefreshReason::WsStageTransition => "ws-stage-transition",
            CurrentCookRefreshReason::StartCommandAccepted => "start-command-accepted",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum HistoryRefreshReason {
    WsIdleToCooking,
    WsCookToIdle,
}

impl HistoryRefreshReason {
    fn as_str(self) -> &'static str {
        match self {
            HistoryRefreshReason::WsIdleToCooking => "ws-idle-to-cooking",
            HistoryRefreshReason::WsCookToIdle => "ws-cook-to-idle",
        }
    }
}

struct AppState {
    /// Latest oven status from the WebSocket stream.
    status_rx: watch::Receiver<Option<anova_oven_api::OvenStatus>>,
    /// Cooker ID received from EVENT_APO_WIFI_LIST, needed to address commands.
    cooker_id: Mutex<Option<String>>,
    /// Send commands to the WebSocket background task.
    cmd_tx: mpsc::Sender<WsCommand>,
    /// Cached current cook; refreshed in the background to avoid request-time Firestore reads.
    current_cook_rx: watch::Receiver<Option<anova_oven_api::CurrentCook>>,
    /// Trigger immediate current-cook refreshes (best effort).
    current_cook_refresh_tx: mpsc::Sender<CurrentCookRefreshReason>,
    /// Cached recipe list, refreshed in the background periodically.
    recipes: Mutex<Option<Vec<anova_oven_api::Recipe>>>,
    /// Cached history list, refreshed in the background periodically.
    history: Mutex<Option<Vec<anova_oven_api::HistoryEntry>>>,
    /// Trigger immediate history refreshes (best effort).
    history_refresh_tx: mpsc::Sender<HistoryRefreshReason>,
    /// Firebase session (ID token + UID), refreshed as needed.
    session: Mutex<firestore::FirebaseSession>,
    http: reqwest::Client,
    current_cook_timeout: Duration,
    current_cook_resolution_timeout: Duration,
    current_cook_refresh_interval: Duration,
    recipes_refresh_interval: Duration,
    history_refresh_interval: Duration,
    history_on_transition_max_wait: Duration,
    history_on_transition_poll_interval: Duration,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

fn env_duration_secs(var: &str, default_secs: u64) -> Duration {
    match std::env::var(var) {
        Ok(value) => match value.parse::<u64>() {
            Ok(secs) => Duration::from_secs(secs),
            Err(err) => {
                warn!(
                    env_var = var,
                    value = %value,
                    error = %err,
                    default_secs,
                    "Invalid duration; using default"
                );
                Duration::from_secs(default_secs)
            }
        },
        Err(_) => Duration::from_secs(default_secs),
    }
}

fn init_tracing() -> WorkerGuard {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "anova_oven_server=info,anova_oven_server::ws=debug,anova_oven_server::firestore=debug",
        )
    });

    let (non_blocking, guard) = tracing_appender::non_blocking::NonBlockingBuilder::default()
        .lossy(true)
        .finish(std::io::stderr());

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(non_blocking),
        )
        .init();

    guard
}

#[tokio::main]
async fn main() {
    let _tracing_guard = init_tracing();
    let anova_token = std::env::var("ANOVA_TOKEN").expect("ANOVA_TOKEN env var is required");
    let anova_email = std::env::var("ANOVA_EMAIL").expect("ANOVA_EMAIL env var is required");
    let anova_password =
        std::env::var("ANOVA_PASSWORD").expect("ANOVA_PASSWORD env var is required");

    let http = reqwest::Client::builder()
        .connect_timeout(env_duration_secs(
            "ANOVA_HTTP_CONNECT_TIMEOUT_SECS",
            DEFAULT_HTTP_CONNECT_TIMEOUT_SECS,
        ))
        .timeout(env_duration_secs(
            "ANOVA_HTTP_TIMEOUT_SECS",
            DEFAULT_HTTP_TIMEOUT_SECS,
        ))
        .build()
        .expect("Failed to build HTTP client");

    info!("Signing into Firebase...");
    let session = tokio::time::timeout(
        Duration::from_secs(20),
        firestore::sign_in(&http, &anova_email, &anova_password),
    )
    .await
    .expect("Firebase sign-in timed out after 20s")
    .expect("Firebase sign-in failed");
    info!(uid = %session.uid, "Signed into Firebase");

    let (status_tx, status_rx) = watch::channel(None::<anova_oven_api::OvenStatus>);
    let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>(8);
    let (current_cook_tx, current_cook_rx) = watch::channel(None::<anova_oven_api::CurrentCook>);
    let (current_cook_refresh_tx, current_cook_refresh_rx) =
        mpsc::channel::<CurrentCookRefreshReason>(16);
    let (history_refresh_tx, history_refresh_rx) = mpsc::channel::<HistoryRefreshReason>(16);

    let state = Arc::new(AppState {
        status_rx,
        cooker_id: Mutex::new(None),
        cmd_tx,
        current_cook_rx,
        current_cook_refresh_tx,
        recipes: Mutex::new(None),
        history: Mutex::new(None),
        history_refresh_tx,
        session: Mutex::new(session),
        http,
        current_cook_timeout: env_duration_secs(
            "ANOVA_CURRENT_COOK_TIMEOUT_SECS",
            DEFAULT_CURRENT_COOK_TIMEOUT_SECS,
        ),
        current_cook_resolution_timeout: env_duration_secs(
            "ANOVA_CURRENT_COOK_RESOLUTION_TIMEOUT_SECS",
            DEFAULT_CURRENT_COOK_RESOLUTION_TIMEOUT_SECS,
        ),
        current_cook_refresh_interval: env_duration_secs(
            "ANOVA_CURRENT_COOK_REFRESH_INTERVAL_SECS",
            DEFAULT_CURRENT_COOK_REFRESH_INTERVAL_SECS,
        ),
        recipes_refresh_interval: env_duration_secs(
            "ANOVA_RECIPES_REFRESH_INTERVAL_SECS",
            DEFAULT_RECIPES_REFRESH_INTERVAL_SECS,
        ),
        history_refresh_interval: env_duration_secs(
            "ANOVA_HISTORY_REFRESH_INTERVAL_SECS",
            DEFAULT_HISTORY_REFRESH_INTERVAL_SECS,
        ),
        history_on_transition_max_wait: env_duration_secs(
            "ANOVA_HISTORY_ON_TRANSITION_MAX_WAIT_SECS",
            DEFAULT_HISTORY_ON_TRANSITION_MAX_WAIT_SECS,
        ),
        history_on_transition_poll_interval: env_duration_secs(
            "ANOVA_HISTORY_ON_TRANSITION_POLL_INTERVAL_SECS",
            DEFAULT_HISTORY_ON_TRANSITION_POLL_INTERVAL_SECS,
        ),
    });

    match refresh_recipes_cache_with_timeout(&state).await {
        Ok(recipes) => info!(count = recipes.len(), "[recipes] preloaded"),
        Err(e) => warn!(error = %e, "[recipes] startup preload failed"),
    }
    match refresh_history_cache_with_timeout(&state).await {
        Ok(entries) => info!(count = entries.len(), "[history] preloaded"),
        Err(e) => warn!(error = %e, "[history] startup preload failed"),
    }

    // Spawn WebSocket background task.
    let ws_token = anova_token.clone();
    tokio::spawn(ws_task(
        ws_token,
        status_tx,
        state.clone(),
        current_cook_tx.clone(),
        cmd_rx,
    ));

    // Spawn periodic + event-driven current-cook cache refresher.
    tokio::spawn(current_cook_refresh_task(
        state.clone(),
        current_cook_tx,
        current_cook_refresh_rx,
    ));

    // Spawn periodic history and recipes cache refreshers.
    tokio::spawn(history_refresh_task(state.clone(), history_refresh_rx));
    tokio::spawn(recipes_refresh_task(state.clone()));

    let app = Router::new()
        .route("/status", axum::routing::get(handle_status))
        .route("/recipes", axum::routing::get(handle_recipes))
        .route(
            "/update-recipes",
            axum::routing::post(handle_update_recipes),
        )
        .route("/history", axum::routing::get(handle_history))
        .route("/stop", axum::routing::post(handle_stop))
        .route("/start", axum::routing::post(handle_start))
        .route("/current-cook", axum::routing::get(handle_current_cook))
        .with_state(state);

    let addr = std::env::var("ANOVA_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");
    info!(address = %addr, "HTTP server listening");
    axum::serve(listener, app).await.expect("Server error");
}

// ─── WebSocket background task ─────────────────────────────────────────────────

fn stop_command_json(cooker_id: &str) -> String {
    let request_id = Uuid::new_v4();
    serde_json::json!({
        "command": "CMD_APO_STOP",
        "payload": {
            "type": "CMD_APO_STOP",
            "id": cooker_id
        },
        "requestId": request_id
    })
    .to_string()
}

fn start_command_json(cooker_id: &str, cook_id: &str, stages: &[anova_oven_api::Stage]) -> String {
    let request_id = Uuid::new_v4();

    // Convert recipe stages to WebSocket stage format
    let ws_stages: Vec<serde_json::Value> = stages
        .iter()
        .map(|stage| {
            let stage_id = format!("stage-{}", Uuid::new_v4());

            // Apply sensible defaults for fan speed (Anova requires fan to run)
            let fan_speed = if stage.fan_speed == 0 {
                75
            } else {
                stage.fan_speed
            };

            let temperature_bulbs_mode = stage.temperature_bulbs_mode.as_deref().unwrap_or("dry");
            let setpoint = serde_json::json!({
                "celsius": stage.temperature_c,
                "fahrenheit": stage.temperature_c * 1.8 + 32.0
            });

            let temperature_bulbs = if temperature_bulbs_mode == "wet" {
                serde_json::json!({
                    "mode": "wet",
                    "wet": { "setpoint": setpoint }
                })
            } else {
                serde_json::json!({
                    "mode": "dry",
                    "dry": { "setpoint": setpoint }
                })
            };

            // Build the stage object with all fields from the recipe
            let mut stage_obj = serde_json::json!({
                "stepType": "stage",
                "id": stage_id,
                "title": stage.title.as_deref().unwrap_or(""),
                "type": stage.kind.as_str(),
                "userActionRequired": stage.user_action_required.unwrap_or(false),
                "stageTransitionType": "automatic",
                "temperatureBulbs": temperature_bulbs,
                "heatingElements": {
                    "top": { "on": stage.heating_element_top.unwrap_or(true) },
                    "rear": { "on": stage.heating_element_rear.unwrap_or(true) },
                    "bottom": { "on": stage.heating_element_bottom.unwrap_or(true) }
                },
                "fan": { "speed": fan_speed },
                "vent": { "open": stage.vent_open.unwrap_or(false) },
                "rackPosition": stage.rack_position.unwrap_or(3),
                "steamGenerators": {
                    "mode": "steam-percentage",
                    "steamPercentage": { "setpoint": stage.steam_pct }
                },
                "timerAdded": false,
                "probeAdded": false
            });

            // Add timer or probe based on stage configuration
            if let Some(duration) = stage.duration_secs {
                stage_obj["timerAdded"] = serde_json::json!(true);
                stage_obj["timer"] = serde_json::json!({ "initial": duration });
                stage_obj["probeAdded"] = serde_json::json!(false);
            } else if let Some(probe_target_c) = stage.probe_target_c {
                stage_obj["timerAdded"] = serde_json::json!(false);
                stage_obj["probeAdded"] = serde_json::json!(true);
                stage_obj["temperatureProbe"] = serde_json::json!({
                    "setpoint": {
                        "celsius": probe_target_c,
                        "fahrenheit": probe_target_c * 1.8 + 32.0
                    }
                });
            }

            stage_obj
        })
        .collect();

    serde_json::json!({
        "command": "CMD_APO_START",
        "payload": {
            "type": "CMD_APO_START",
            "id": cooker_id,
            "payload": {
                "cookId": cook_id,
                "stages": ws_stages
            }
        },
        "requestId": request_id
    })
    .to_string()
}

async fn ws_task(
    token: String,
    status_tx: watch::Sender<Option<anova_oven_api::OvenStatus>>,
    state: Arc<AppState>,
    current_cook_tx: watch::Sender<Option<anova_oven_api::CurrentCook>>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
) {
    loop {
        info!("[ws] connecting to Anova WebSocket");
        match ws_connect_and_run(&token, &status_tx, &state, &current_cook_tx, &mut cmd_rx).await {
            Ok(()) => info!("[ws] connection closed cleanly"),
            Err(e) => warn!(error = %e, "[ws] connection error"),
        }
        info!("[ws] reconnecting in 5s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn ws_connect_and_run(
    token: &str,
    status_tx: &watch::Sender<Option<anova_oven_api::OvenStatus>>,
    state: &Arc<AppState>,
    current_cook_tx: &watch::Sender<Option<anova_oven_api::CurrentCook>>,
    cmd_rx: &mut mpsc::Receiver<WsCommand>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let uri = Uri::builder()
        .scheme("wss")
        .authority("devices.anovaculinary.io")
        .path_and_query(format!(
            "/?token={token}&supportedAccessories=APO&platform=android"
        ))
        .build()?;

    let (ws, _) = ClientBuilder::from_uri(uri)
        .add_header(
            HeaderName::from_static("sec-websocket-protocol"),
            HeaderValue::from_static("ANOVA_V2"),
        )?
        .connect()
        .await?;

    info!("[ws] connected");

    let mut was_cooking = false;
    let mut seen_state = false;
    let mut prev_timer_initial: Option<u64> = None;
    let (mut sink, mut stream) = ws.split();

    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        let payload = msg.as_payload();
                        match protocol::parse_message(payload) {
                            Ok(protocol::Event::ApoState(payload)) => {
                                let status = protocol::to_oven_status(&payload);
                                let is_cooking = status.mode != "idle";
                                let timer_initial = status.timer_total_secs;

                                if is_cooking && !was_cooking {
                                    let reason = if seen_state {
                                        CurrentCookRefreshReason::WsIdleToCooking
                                    } else {
                                        CurrentCookRefreshReason::WsConnectedCookingState
                                    };
                                    request_current_cook_refresh(&state.current_cook_refresh_tx, reason);
                                    request_history_refresh(
                                        &state.history_refresh_tx,
                                        HistoryRefreshReason::WsIdleToCooking,
                                    );
                                }

                                if !is_cooking && was_cooking {
                                    request_history_refresh(
                                        &state.history_refresh_tx,
                                        HistoryRefreshReason::WsCookToIdle,
                                    );
                                }

                                // Detect stage transitions: timer_initial changes while
                                // continuously cooking indicates the oven moved to a new stage.
                                if is_cooking && was_cooking {
                                    if let Some(prev) = prev_timer_initial {
                                        if prev != timer_initial {
                                            request_current_cook_refresh(
                                                &state.current_cook_refresh_tx,
                                                CurrentCookRefreshReason::WsStageTransition,
                                            );
                                        }
                                    }
                                }

                                if !is_cooking {
                                    // Clear stale cache immediately when oven returns to idle.
                                    let _ = current_cook_tx.send(None);
                                    prev_timer_initial = None;
                                } else {
                                    prev_timer_initial = Some(timer_initial);
                                }

                                info!(
                                    mode = %status.mode,
                                    temp_f = celcius_to_fahrenheit(status.temperature_c),
                                    steam_current_pct = status.steam_pct,
                                    steam_target_pct = status.steam_target_pct.unwrap_or(0.0),
                                    probe_f = celcius_to_fahrenheit(status.probe_temperature_c.unwrap_or(0.0)),
                                    "[ws] state"
                                );
                                let _ = status_tx.send(Some(status));
                                was_cooking = is_cooking;
                                seen_state = true;
                            }
                            Ok(protocol::Event::ApoWifiList { cooker_id: id }) => {
                                if let Some(id) = id {
                                    info!(cooker_id = %id, "[ws] cooker id received");
                                    let mut cooker_id = state.cooker_id.lock().await;
                                    *cooker_id = Some(id);
                                }
                            }
                            Ok(protocol::Event::Response { request_id, status }) => {
                                debug!(request_id = %request_id, status = %status, "[ws] response");
                            }
                            Ok(event) => {
                                trace!(event = ?event, "[ws] event");
                            }
                            Err(e) => {
                                warn!(error = %e, "[ws] parse error");
                            }
                        }
                    }
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(WsCommand::Stop) => {
                        let cooker_id = state.cooker_id.lock().await.clone();
                        match cooker_id {
                            Some(id) => {
                                info!(cooker_id = %id, "[ws] sending CMD_APO_STOP");
                                sink.send(Message::text(stop_command_json(&id))).await?;
                            }
                            None => warn!("[ws] stop requested before cooker id known"),
                        }
                    }
                    Some(WsCommand::Start { recipe_id, stages }) => {
                        let cooker_id = state.cooker_id.lock().await.clone();
                        match cooker_id {
                            Some(id) => {
                                let cook_id = format!("server-{}", Uuid::new_v4());
                                let cmd_json = start_command_json(&id, &cook_id, &stages);
                                info!(cooker_id = %id, cook_id = %cook_id, "[ws] sending CMD_APO_START");
                                trace!(command_json = %cmd_json, "[ws] start payload");
                                sink.send(Message::text(cmd_json)).await?;
                                request_current_cook_refresh(
                                    &state.current_cook_refresh_tx,
                                    CurrentCookRefreshReason::StartCommandAccepted,
                                );
                                tokio::spawn(set_cook_recipe_ref_with_retry(
                                    state.clone(),
                                    cook_id,
                                    recipe_id,
                                ));
                            }
                            None => warn!("[ws] start requested before cooker id known"),
                        }
                    }
                    None => {} // cmd_tx dropped — server shutting down
                }
            }
        }
    }
}

fn request_current_cook_refresh(
    tx: &mpsc::Sender<CurrentCookRefreshReason>,
    reason: CurrentCookRefreshReason,
) {
    match tx.try_send(reason) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!(
                reason = reason.as_str(),
                "[current-cook] refresh trigger dropped (queue full)"
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            warn!(
                reason = reason.as_str(),
                "[current-cook] refresh trigger dropped (task not running)"
            );
        }
    }
}

fn request_history_refresh(tx: &mpsc::Sender<HistoryRefreshReason>, reason: HistoryRefreshReason) {
    match tx.try_send(reason) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!(
                reason = reason.as_str(),
                "[history] refresh trigger dropped (queue full)"
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            warn!(
                reason = reason.as_str(),
                "[history] refresh trigger dropped (task not running)"
            );
        }
    }
}

async fn current_cook_refresh_task(
    state: Arc<AppState>,
    current_cook_tx: watch::Sender<Option<anova_oven_api::CurrentCook>>,
    mut refresh_rx: mpsc::Receiver<CurrentCookRefreshReason>,
) {
    let mut interval = tokio::time::interval(state.current_cook_refresh_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    refresh_current_cook_cache(&state, &current_cook_tx, CurrentCookRefreshReason::Startup).await;

    loop {
        tokio::select! {
            _ = interval.tick() => {
                let is_cooking = state.status_rx.borrow().as_ref().map(|s| s.is_cooking()).unwrap_or(false);
                if is_cooking {
                    refresh_current_cook_cache(&state, &current_cook_tx, CurrentCookRefreshReason::Periodic).await;
                }
            }
            reason = refresh_rx.recv() => {
                match reason {
                    Some(reason) => {
                        refresh_current_cook_cache(&state, &current_cook_tx, reason).await;
                    }
                    None => {
                        warn!("[current-cook] refresh trigger channel closed");
                        return;
                    }
                }
            }
        }
    }
}

async fn refresh_current_cook_cache(
    state: &AppState,
    current_cook_tx: &watch::Sender<Option<anova_oven_api::CurrentCook>>,
    reason: CurrentCookRefreshReason,
) {
    let result = match tokio::time::timeout(state.current_cook_timeout, async {
        fetch_current_cook_from_firebase(state).await
    })
    .await
    {
        Ok(v) => v,
        Err(_) => {
            warn!(
                timeout_secs = state.current_cook_timeout.as_secs(),
                reason = reason.as_str(),
                "[current-cook] refresh timed out"
            );
            return;
        }
    };

    match result {
        Ok(cook) => {
            let _ = current_cook_tx.send(cook);
        }
        Err(e) => {
            warn!(reason = reason.as_str(), error = %e, "[current-cook] refresh failed");
        }
    }
}

async fn history_refresh_task(
    state: Arc<AppState>,
    mut refresh_rx: mpsc::Receiver<HistoryRefreshReason>,
) {
    let mut interval = tokio::time::interval(state.history_refresh_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // startup preload already done in main

    loop {
        tokio::select! {
            _ = interval.tick() => {
                match refresh_history_cache_with_timeout(&state).await {
                    Ok(entries) => info!(count = entries.len(), "[history] periodic refresh"),
                    Err(e) => warn!(error = %e, "[history] periodic refresh failed"),
                }
            }
            reason = refresh_rx.recv() => {
                match reason {
                    Some(reason) => {
                        let refresh_result =
                            refresh_history_until_new_entry_after_transition(&state, reason).await;

                        match refresh_result {
                            Ok(entries) => info!(reason = reason.as_str(), count = entries.len(), "[history] refreshed"),
                            Err(e) => warn!(reason = reason.as_str(), error = %e, "[history] refresh failed"),
                        }
                    }
                    None => {
                        warn!("[history] refresh trigger channel closed");
                        return;
                    }
                }
            }
        }
    }
}

fn history_head_signature(entries: &[anova_oven_api::HistoryEntry]) -> Option<String> {
    entries.first().map(|entry| {
        format!(
            "{}|{}|{}",
            entry.ended_at, entry.recipe_title, entry.stage_count
        )
    })
}

async fn refresh_history_cache_with_timeout(
    state: &AppState,
) -> Result<Vec<anova_oven_api::HistoryEntry>, String> {
    match tokio::time::timeout(
        Duration::from_secs(DEFAULT_HISTORY_REFRESH_TIMEOUT_SECS),
        refresh_history_cache(state),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "History refresh timed out after {}s",
            DEFAULT_HISTORY_REFRESH_TIMEOUT_SECS
        )),
    }
}

async fn refresh_history_until_new_entry_after_transition(
    state: &AppState,
    reason: HistoryRefreshReason,
) -> Result<Vec<anova_oven_api::HistoryEntry>, String> {
    let baseline_head = {
        let cache = state.history.lock().await;
        cache
            .as_ref()
            .and_then(|history| history_head_signature(history.as_slice()))
    };

    let poll_interval = if state.history_on_transition_poll_interval.is_zero() {
        Duration::from_secs(1)
    } else {
        state.history_on_transition_poll_interval
    };
    let deadline = tokio::time::Instant::now() + state.history_on_transition_max_wait;

    let mut attempts: u32 = 0;
    let mut last_entries: Option<Vec<anova_oven_api::HistoryEntry>> = None;
    let mut last_error: Option<String> = None;

    loop {
        attempts += 1;

        match refresh_history_cache_with_timeout(state).await {
            Ok(entries) => {
                let refreshed_head = history_head_signature(entries.as_slice());
                let changed = baseline_head != refreshed_head;
                let should_accept = baseline_head.is_none() || changed;

                if should_accept {
                    info!(
                        attempts,
                        reason = reason.as_str(),
                        baseline_head = ?baseline_head,
                        refreshed_head = ?refreshed_head,
                        "[history] transition refresh observed updated history"
                    );
                    return Ok(entries);
                }

                last_entries = Some(entries);
            }
            Err(e) => {
                last_error = Some(e);
            }
        }

        if tokio::time::Instant::now() >= deadline {
            if let Some(entries) = last_entries {
                info!(
                    attempts,
                    reason = reason.as_str(),
                    "[history] transition refresh window elapsed without new head entry"
                );
                return Ok(entries);
            }

            let error = last_error.unwrap_or_else(|| {
                format!(
                    "History refresh did not succeed before transition wait elapsed ({})",
                    reason.as_str()
                )
            });
            return Err(error);
        }

        tokio::time::sleep(poll_interval).await;
    }
}

async fn recipes_refresh_task(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(state.recipes_refresh_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // startup preload already done in main

    loop {
        interval.tick().await;
        match refresh_recipes_cache_with_timeout(&state).await {
            Ok(recipes) => info!(count = recipes.len(), "[recipes] periodic refresh"),
            Err(e) => warn!(error = %e, "[recipes] periodic refresh failed"),
        }
    }
}

// ─── HTTP handlers ─────────────────────────────────────────────────────────────

fn build_response(status: StatusCode, content_type: &'static str, body: Vec<u8>) -> Response {
    let content_length = body.len().to_string();

    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header(CONTENT_LENGTH, content_length)
        .header(CONNECTION, "close")
        .body(Body::from(body))
        .expect("failed to build HTTP response")
}

fn text_response(status: StatusCode, message: impl Into<String>) -> Response {
    build_response(
        status,
        "text/plain; charset=utf-8",
        message.into().into_bytes(),
    )
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => build_response(status, "application/json", body),
        Err(err) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to serialize JSON response: {err}"),
        ),
    }
}

fn empty_response(status: StatusCode) -> Response {
    build_response(status, "text/plain; charset=utf-8", Vec::new())
}

async fn handle_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    trace!("[http] GET /status");
    match state.status_rx.borrow().clone() {
        Some(status) => {
            trace!("[http] GET /status -> OK");
            json_response(StatusCode::OK, &status)
        }
        None => text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Oven state not yet received — WebSocket may still be connecting",
        ),
    }
}

async fn handle_recipes(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    trace!("[http] GET /recipes");
    let cache = state.recipes.lock().await;
    trace!("[http] GET /recipes -> locked cache");
    match &*cache {
        Some(recipes) => {
            trace!(count = recipes.len(), "[http] GET /recipes -> OK");
            json_response(StatusCode::OK, recipes)
        }
        None => {
            debug!("[http] GET /recipes -> SERVICE_UNAVAILABLE");
            text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Recipe cache not yet populated — server may still be starting up",
            )
        }
    }
}

async fn handle_update_recipes(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    debug!("[http] POST /update-recipes");
    match refresh_recipes_cache(&state).await {
        Ok(recipes) => {
            info!(count = recipes.len(), "[http] POST /update-recipes -> OK");
            json_response(StatusCode::OK, &recipes)
        }
        Err(e) => {
            warn!(error = %e, "[http] POST /update-recipes -> ERROR");
            text_response(
                StatusCode::BAD_GATEWAY,
                format!("Failed to refresh recipes: {e}"),
            )
        }
    }
}

async fn handle_stop(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    debug!("[http] POST /stop");
    if state.cooker_id.lock().await.is_none() {
        debug!("[http] POST /stop -> SERVICE_UNAVAILABLE (no cooker ID)");
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cooker ID not yet received — WebSocket may still be connecting",
        );
    }
    match state.cmd_tx.send(WsCommand::Stop).await {
        Ok(()) => {
            info!("[http] POST /stop -> NO_CONTENT");
            empty_response(StatusCode::NO_CONTENT)
        }
        Err(_) => {
            warn!("[http] POST /stop -> INTERNAL_SERVER_ERROR");
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "WebSocket task not running",
            )
        }
    }
}

#[derive(Serialize, Deserialize)]
struct StartRequest {
    recipe_id: String,
}

async fn handle_start(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StartRequest>,
) -> impl IntoResponse {
    debug!(recipe_id = %req.recipe_id, "[http] POST /start");
    if state.cooker_id.lock().await.is_none() {
        debug!("[http] POST /start -> SERVICE_UNAVAILABLE (no cooker ID)");
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Cooker ID not yet received — WebSocket may still be connecting",
        );
    }

    // Try cache first. After a server restart, this cache may be empty.
    let cached_recipe = {
        trace!("[http] POST /start -> checking recipes cache");
        let recipes = state.recipes.lock().await;
        trace!("[http] POST /start -> recipes cache locked");
        recipes
            .as_ref()
            .and_then(|recipes| recipes.iter().find(|r| r.id == req.recipe_id).cloned())
    };
    debug!(
        found = cached_recipe.is_some(),
        "[http] POST /start -> cache check complete"
    );

    let recipe = if let Some(recipe) = cached_recipe {
        trace!("[http] POST /start -> using cached recipe");
        Some(recipe)
    } else {
        // Cache miss: hydrate recipes from Firestore so /start works even when
        // the server has just restarted and /recipes has not been called yet.
        debug!("[http] POST /start -> cache miss, refreshing from Firestore");
        match refresh_recipes_cache(&state).await {
            Ok(recipes) => {
                info!(
                    count = recipes.len(),
                    "[http] POST /start -> refresh complete"
                );
                let recipe = recipes.iter().find(|r| r.id == req.recipe_id).cloned();
                recipe
            }
            Err(e) => {
                warn!(error = %e, "[start] failed to refresh recipes after cache miss");
                warn!("[http] POST /start -> BAD_GATEWAY");
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to fetch recipes for start request: {e}"),
                );
            }
        }
    };

    let recipe = match recipe {
        Some(r) => r,
        None => {
            debug!("[http] POST /start -> NOT_FOUND");
            return text_response(
                StatusCode::NOT_FOUND,
                format!("Recipe with ID '{}' not found", req.recipe_id),
            );
        }
    };

    match state
        .cmd_tx
        .send(WsCommand::Start {
            recipe_id: recipe.id,
            stages: recipe.stages,
        })
        .await
    {
        Ok(()) => {
            info!("[http] POST /start -> NO_CONTENT");
            empty_response(StatusCode::NO_CONTENT)
        }
        Err(_) => {
            warn!("[http] POST /start -> INTERNAL_SERVER_ERROR");
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "WebSocket task not running",
            )
        }
    }
}

async fn handle_history(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    trace!("[http] GET /history");
    let cache = state.history.lock().await;
    trace!("[http] GET /history -> locked cache");
    match &*cache {
        Some(history) => {
            trace!(count = history.len(), "[http] GET /history -> OK");
            json_response(StatusCode::OK, history)
        }
        None => {
            debug!("[http] GET /history -> SERVICE_UNAVAILABLE");
            text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "History cache not yet populated — server may still be starting up",
            )
        }
    }
}

fn celcius_to_fahrenheit(c: f32) -> f32 {
    c * 9.0 / 5.0 + 32.0
}

/// If `err` is [`firestore::FirestoreError::Unauthorized`], refreshes the
/// Firebase session in `state` and returns a fresh clone so the caller can
/// retry the failed request. Any other error is converted to a `String` and returned as `Err`.
async fn maybe_refresh_session(
    state: &AppState,
    err: firestore::FirestoreError,
) -> Result<firestore::FirebaseSession, String> {
    match err {
        firestore::FirestoreError::Unauthorized => {
            info!("[auth] Firebase token expired; refreshing");
            let mut refreshed = {
                let locked = state.session.lock().await;
                locked.clone()
            };

            if let Err(e) = firestore::refresh_session(&state.http, &mut refreshed).await {
                return Err(format!("Token refresh failed: {e}"));
            }

            let mut locked = state.session.lock().await;
            *locked = refreshed.clone();
            info!("[auth] Token refreshed successfully");
            Ok(refreshed)
        }
        firestore::FirestoreError::Other(e) => Err(e.to_string()),
    }
}

fn approx_eq_f32(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance
}

fn stage_semantically_matches(a: &anova_oven_api::Stage, b: &anova_oven_api::Stage) -> bool {
    a.kind == b.kind
        && approx_eq_f32(a.temperature_c, b.temperature_c, 1.0)
        && approx_eq_f32(a.steam_pct, b.steam_pct, 2.0)
        && a.duration_secs == b.duration_secs
        && match (a.probe_target_c, b.probe_target_c) {
            (Some(x), Some(y)) => approx_eq_f32(x, y, 1.0),
            (None, None) => true,
            _ => false,
        }
}

fn cook_matches_recipe(
    cook: &anova_oven_api::CurrentCook,
    recipe: &anova_oven_api::Recipe,
) -> bool {
    cook.stages.len() == recipe.stages.len()
        && cook
            .stages
            .iter()
            .zip(recipe.stages.iter())
            .all(|(c, r)| stage_semantically_matches(c, r))
}

fn resolve_title_from_recipes(
    cook: &anova_oven_api::CurrentCook,
    recipes: &[anova_oven_api::Recipe],
) -> Option<(String, String)> {
    if let Some(ref recipe_id) = cook.recipe_id {
        if let Some(recipe) = recipes.iter().find(|r| r.id == *recipe_id) {
            return Some((recipe.title.clone(), recipe.id.clone()));
        }
    }

    let mut matches = recipes.iter().filter(|r| cook_matches_recipe(cook, r));
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some((first.title.clone(), first.id.clone()))
}

async fn recipes_for_resolution(state: &AppState) -> Result<Vec<anova_oven_api::Recipe>, String> {
    {
        let cache = state.recipes.lock().await;
        if let Some(ref recipes) = *cache {
            return Ok(recipes.clone());
        }
    }

    refresh_recipes_cache(state).await
}

async fn handle_current_cook(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.current_cook_rx.borrow().clone() {
        Some(cook) => json_response(StatusCode::OK, &cook),
        None => empty_response(StatusCode::NO_CONTENT),
    }
}

async fn fetch_recipes_with_retry(state: &AppState) -> Result<Vec<anova_oven_api::Recipe>, String> {
    let session = state.session.lock().await.clone();
    match firestore::fetch_recipes(&state.http, &session).await {
        Ok(v) => Ok(v),
        Err(err) => match maybe_refresh_session(state, err).await {
            Ok(fresh) => firestore::fetch_recipes(&state.http, &fresh)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        },
    }
}

async fn refresh_recipes_cache(state: &AppState) -> Result<Vec<anova_oven_api::Recipe>, String> {
    let recipes = fetch_recipes_with_retry(state).await?;
    let mut cache = state.recipes.lock().await;
    *cache = Some(recipes.clone());
    Ok(recipes)
}

async fn refresh_recipes_cache_with_timeout(
    state: &AppState,
) -> Result<Vec<anova_oven_api::Recipe>, String> {
    match tokio::time::timeout(
        Duration::from_secs(DEFAULT_RECIPES_REFRESH_TIMEOUT_SECS),
        refresh_recipes_cache(state),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Recipes refresh timed out after {}s",
            DEFAULT_RECIPES_REFRESH_TIMEOUT_SECS
        )),
    }
}

async fn refresh_history_cache(
    state: &AppState,
) -> Result<Vec<anova_oven_api::HistoryEntry>, String> {
    let session = state.session.lock().await.clone();
    let result = match firestore::fetch_history(&state.http, &session, 50).await {
        Ok(v) => Ok(v),
        Err(err) => match maybe_refresh_session(state, err).await {
            Ok(fresh) => firestore::fetch_history(&state.http, &fresh, 50)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        },
    };
    match result {
        Ok(history) => {
            let mut cache = state.history.lock().await;
            *cache = Some(history.clone());
            Ok(history)
        }
        Err(e) => Err(e),
    }
}

/// Best-effort background task: after a `CMD_APO_START` is sent, set
/// `recipeRef` on the `users/{uid}/oven-cooks/{cook_id}` Firestore document so
/// that phones recognize the cook as a specific recipe. Anova's cloud creates
/// the document asynchronously from the WebSocket command, so we retry a few
/// times with a short delay to win the race without blocking `/start`.
async fn set_cook_recipe_ref_with_retry(state: Arc<AppState>, cook_id: String, recipe_id: String) {
    const ATTEMPTS: u32 = 4;
    const INITIAL_DELAY_MS: u64 = 500;
    const RETRY_DELAY_MS: u64 = 750;

    tokio::time::sleep(Duration::from_millis(INITIAL_DELAY_MS)).await;

    for attempt in 1..=ATTEMPTS {
        let session = state.session.lock().await.clone();
        let result =
            match firestore::patch_cook_recipe_ref(&state.http, &session, &cook_id, &recipe_id)
                .await
            {
                Ok(()) => Ok(()),
                Err(err) => match maybe_refresh_session(&state, err).await {
                    Ok(fresh) => {
                        firestore::patch_cook_recipe_ref(&state.http, &fresh, &cook_id, &recipe_id)
                            .await
                            .map_err(|e| e.to_string())
                    }
                    Err(e) => Err(e),
                },
            };

        match result {
            Ok(()) => {
                info!(
                    cook_id = %cook_id,
                    recipe_id = %recipe_id,
                    attempt,
                    "[current-cook] recipeRef patched"
                );
                request_current_cook_refresh(
                    &state.current_cook_refresh_tx,
                    CurrentCookRefreshReason::StartCommandAccepted,
                );
                return;
            }
            Err(e) => {
                warn!(
                    cook_id = %cook_id,
                    recipe_id = %recipe_id,
                    attempt,
                    error = %e,
                    "[current-cook] recipeRef patch attempt failed"
                );
            }
        }

        if attempt < ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
        }
    }

    warn!(
        cook_id = %cook_id,
        recipe_id = %recipe_id,
        "[current-cook] recipeRef patch gave up after all retries"
    );
}

async fn fetch_current_cook_from_firebase(
    state: &AppState,
) -> Result<Option<anova_oven_api::CurrentCook>, String> {
    let session = state.session.lock().await.clone();
    let mut cook = match firestore::fetch_current_cook(&state.http, &session).await {
        Ok(v) => v,
        Err(err) => match maybe_refresh_session(state, err).await {
            Ok(fresh) => firestore::fetch_current_cook(&state.http, &fresh)
                .await
                .map_err(|e| e.to_string())?,
            Err(e) => return Err(e),
        },
    };

    if let Some(ref mut cook_value) = cook {
        if cook_value.recipe_title == "[manual]" {
            match tokio::time::timeout(
                state.current_cook_resolution_timeout,
                recipes_for_resolution(state),
            )
            .await
            {
                Ok(Ok(recipes)) => {
                    if let Some((title, id)) = resolve_title_from_recipes(cook_value, &recipes) {
                        debug!(title = %title, recipe_id = %id, "[current-cook] resolved title from stage match");
                        cook_value.recipe_title = title;
                        if cook_value.recipe_id.is_none() {
                            cook_value.recipe_id = Some(id);
                        }
                    }
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "[current-cook] recipe resolution skipped");
                }
                Err(_) => {
                    warn!(
                        timeout_secs = state.current_cook_resolution_timeout.as_secs(),
                        "[current-cook] recipe resolution skipped: timeout"
                    );
                }
            }
        }
    }

    Ok(cook)
}
