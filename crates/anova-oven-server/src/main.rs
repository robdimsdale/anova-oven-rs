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
//! - `POST /stop`        — stop the current cook
//! - `POST /start`       — start a cook with a recipe (recipe ID in JSON body)

mod firestore;
mod protocol;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use http::{HeaderName, HeaderValue, Uri};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_websockets::{ClientBuilder, Message};
use uuid::Uuid;

// ─── Shared application state ─────────────────────────────────────────────────

enum WsCommand {
    Stop,
    Start(Vec<anova_oven_api::Stage>),
}

struct AppState {
    /// Latest oven status from the WebSocket stream.
    status_rx: watch::Receiver<Option<anova_oven_api::OvenStatus>>,
    /// Cooker ID received from EVENT_APO_WIFI_LIST, needed to address commands.
    cooker_id_rx: watch::Receiver<Option<String>>,
    /// Send commands to the WebSocket background task.
    cmd_tx: mpsc::Sender<WsCommand>,
    /// Cached recipe list, fetched from Firestore on first request.
    recipes: Mutex<Option<Vec<anova_oven_api::Recipe>>>,
    /// Cached history list, fetched from Firestore on first request.
    history: Mutex<Option<Vec<anova_oven_api::HistoryEntry>>>,
    /// Firebase session (ID token + UID), refreshed as needed.
    session: Mutex<firestore::FirebaseSession>,
    http: reqwest::Client,
}

// ─── Entry point ──────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let anova_token = std::env::var("ANOVA_TOKEN").expect("ANOVA_TOKEN env var is required");
    let anova_email = std::env::var("ANOVA_EMAIL").expect("ANOVA_EMAIL env var is required");
    let anova_password =
        std::env::var("ANOVA_PASSWORD").expect("ANOVA_PASSWORD env var is required");

    let http = reqwest::Client::new();

    eprintln!("Signing into Firebase...");
    let session = firestore::sign_in(&http, &anova_email, &anova_password)
        .await
        .expect("Firebase sign-in failed");
    eprintln!("Signed in as UID {}", session.uid);

    let (status_tx, status_rx) = watch::channel(None::<anova_oven_api::OvenStatus>);
    let (cooker_id_tx, cooker_id_rx) = watch::channel(None::<String>);
    let (cmd_tx, cmd_rx) = mpsc::channel::<WsCommand>(8);

    let state = Arc::new(AppState {
        status_rx,
        cooker_id_rx,
        cmd_tx,
        recipes: Mutex::new(None),
        history: Mutex::new(None),
        session: Mutex::new(session),
        http,
    });

    // Spawn WebSocket background task.
    let ws_token = anova_token.clone();
    tokio::spawn(ws_task(ws_token, status_tx, cooker_id_tx, cmd_rx));

    let app = Router::new()
        .route("/status", axum::routing::get(handle_status))
        .route("/recipes", axum::routing::get(handle_recipes))
        .route("/history", axum::routing::get(handle_history))
        .route("/stop", axum::routing::post(handle_stop))
        .route("/start", axum::routing::post(handle_start))
        .route("/current-cook", axum::routing::get(handle_current_cook))
        .with_state(state);

    let addr = std::env::var("ANOVA_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");
    eprintln!("Listening on http://{addr}");
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

fn start_command_json(cooker_id: &str, stages: &[anova_oven_api::Stage]) -> String {
    let request_id = Uuid::new_v4();
    let cook_id = format!("server-{}", Uuid::new_v4());

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
    cooker_id_tx: watch::Sender<Option<String>>,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
) {
    loop {
        eprintln!("[ws] Connecting to Anova WebSocket...");
        match ws_connect_and_run(&token, &status_tx, &cooker_id_tx, &mut cmd_rx).await {
            Ok(()) => eprintln!("[ws] Connection closed cleanly."),
            Err(e) => eprintln!("[ws] Connection error: {e}"),
        }
        eprintln!("[ws] Reconnecting in 5 s...");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn ws_connect_and_run(
    token: &str,
    status_tx: &watch::Sender<Option<anova_oven_api::OvenStatus>>,
    cooker_id_tx: &watch::Sender<Option<String>>,
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

    eprintln!("[ws] Connected.");

    let mut cooker_id: Option<String> = None;
    let (mut sink, mut stream) = ws.split();

    loop {
        tokio::select! {
            msg = stream.next() => {
                match msg {
                    Some(Ok(msg)) => {
                        let payload = msg.as_payload();
                        match protocol::parse_message(payload) {
                            Ok(protocol::Event::ApoState(state)) => {
                                let status = protocol::to_oven_status(&state);
                                eprintln!("[ws] State: mode={} temp={:.1}°F steam={:.0}%/{:.0}% probe={:.1}°F",
                                status.mode, celcius_to_fahrenheit(status.temperature_c), status.steam_pct, status.steam_target_pct.unwrap_or(0.0), celcius_to_fahrenheit(status.probe_temperature_c.unwrap_or(0.0)) );
                                let _ = status_tx.send(Some(status));
                            }
                            Ok(protocol::Event::ApoWifiList { cooker_id: id }) => {
                                if let Some(id) = id {
                                    eprintln!("[ws] Cooker ID: {id}");
                                    let _ = cooker_id_tx.send(Some(id.clone()));
                                    cooker_id = Some(id);
                                }
                            }
                            Ok(protocol::Event::Response { request_id, status }) => {
                                eprintln!("[ws] Response: request_id={} status={}", request_id, status);
                            }
                            Ok(event) => {
                                eprintln!("[ws] Event: {:?}", event);
                            }
                            Err(e) => {
                                eprintln!("[ws] Parse error: {}", e);
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
                        match &cooker_id {
                            Some(id) => {
                                eprintln!("[ws] Sending CMD_APO_STOP for cooker {id}");
                                sink.send(Message::text(stop_command_json(id))).await?;
                            }
                            None => eprintln!("[ws] Stop requested but cooker ID not yet known"),
                        }
                    }
                    Some(WsCommand::Start(stages)) => {
                        match &cooker_id {
                            Some(id) => {
                                let cmd_json = start_command_json(id, &stages);
                                eprintln!("[ws] Sending CMD_APO_START for cooker {id}");
                                eprintln!("[ws] Command JSON: {}", cmd_json);
                                sink.send(Message::text(cmd_json)).await?;
                            }
                            None => eprintln!("[ws] Start requested but cooker ID not yet known"),
                        }
                    }
                    None => {} // cmd_tx dropped — server shutting down
                }
            }
        }
    }
}

// ─── HTTP handlers ─────────────────────────────────────────────────────────────

async fn handle_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.status_rx.borrow().clone() {
        Some(status) => Json(status).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Oven state not yet received — WebSocket may still be connecting",
        )
            .into_response(),
    }
}

async fn handle_recipes(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    {
        let cache = state.recipes.lock().await;
        if let Some(ref recipes) = *cache {
            return Json(recipes.clone()).into_response();
        }
    }

    let session = state.session.lock().await.clone();
    let result = match firestore::fetch_recipes(&state.http, &session).await {
        Ok(v) => Ok(v),
        Err(err) => match maybe_refresh_session(&state, err).await {
            Ok(fresh) => firestore::fetch_recipes(&state.http, &fresh)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        },
    };

    match result {
        Ok(recipes) => {
            let mut cache = state.recipes.lock().await;
            *cache = Some(recipes.clone());
            Json(recipes).into_response()
        }
        Err(e) => {
            eprintln!("[recipes] Fetch error: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch recipes: {e}"),
            )
                .into_response()
        }
    }
}

async fn handle_stop(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    if state.cooker_id_rx.borrow().is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Cooker ID not yet received — WebSocket may still be connecting",
        )
            .into_response();
    }
    match state.cmd_tx.send(WsCommand::Stop).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "WebSocket task not running",
        )
            .into_response(),
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
    if state.cooker_id_rx.borrow().is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Cooker ID not yet received — WebSocket may still be connecting",
        )
            .into_response();
    }

    // Try cache first. After a server restart, this cache may be empty.
    let cached_recipe = {
        let recipes = state.recipes.lock().await;
        recipes
            .as_ref()
            .and_then(|recipes| recipes.iter().find(|r| r.id == req.recipe_id).cloned())
    };

    let recipe = if let Some(recipe) = cached_recipe {
        Some(recipe)
    } else {
        // Cache miss: hydrate recipes from Firestore so /start works even when
        // the server has just restarted and /recipes has not been called yet.
        let session = state.session.lock().await.clone();
        let fetched = match firestore::fetch_recipes(&state.http, &session).await {
            Ok(v) => Ok(v),
            Err(err) => match maybe_refresh_session(&state, err).await {
                Ok(fresh) => firestore::fetch_recipes(&state.http, &fresh)
                    .await
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            },
        };

        match fetched {
            Ok(recipes) => {
                let recipe = recipes.iter().find(|r| r.id == req.recipe_id).cloned();
                let mut cache = state.recipes.lock().await;
                *cache = Some(recipes);
                recipe
            }
            Err(e) => {
                eprintln!("[start] Failed to refresh recipes after cache miss: {e}");
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("Failed to fetch recipes for start request: {e}"),
                )
                    .into_response();
            }
        }
    };

    let recipe = match recipe {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                format!("Recipe with ID '{}' not found", req.recipe_id),
            )
                .into_response();
        }
    };

    match state.cmd_tx.send(WsCommand::Start(recipe.stages)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "WebSocket task not running",
        )
            .into_response(),
    }
}

async fn handle_history(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    {
        let cache = state.history.lock().await;
        if let Some(ref history) = *cache {
            return Json(history.clone()).into_response();
        }
    }

    let session = state.session.lock().await.clone();
    let result = match firestore::fetch_history(&state.http, &session, 50).await {
        Ok(v) => Ok(v),
        Err(err) => match maybe_refresh_session(&state, err).await {
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
            Json(history).into_response()
        }
        Err(e) => {
            eprintln!("[history] Fetch error: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch history: {e}"),
            )
                .into_response()
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
            eprintln!("[auth] Firebase token expired — refreshing...");
            let mut locked = state.session.lock().await;
            if let Err(e) = firestore::refresh_session(&state.http, &mut locked).await {
                return Err(format!("Token refresh failed: {e}"));
            }
            eprintln!("[auth] Token refreshed successfully.");
            Ok(locked.clone())
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

    let session = state.session.lock().await.clone();
    let fetched = match firestore::fetch_recipes(&state.http, &session).await {
        Ok(v) => Ok(v),
        Err(err) => match maybe_refresh_session(state, err).await {
            Ok(fresh) => firestore::fetch_recipes(&state.http, &fresh)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        },
    }?;

    let mut cache = state.recipes.lock().await;
    *cache = Some(fetched.clone());
    Ok(fetched)
}

async fn handle_current_cook(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let session = state.session.lock().await.clone();
    let result = match firestore::fetch_current_cook(&state.http, &session).await {
        Ok(v) => Ok(v),
        Err(err) => match maybe_refresh_session(&state, err).await {
            Ok(fresh) => firestore::fetch_current_cook(&state.http, &fresh)
                .await
                .map_err(|e| e.to_string()),
            Err(e) => Err(e),
        },
    };

    match result {
        Ok(Some(mut cook)) => {
            if cook.recipe_title == "[custom]" {
                match recipes_for_resolution(&state).await {
                    Ok(recipes) => {
                        if let Some((title, id)) = resolve_title_from_recipes(&cook, &recipes) {
                            eprintln!(
                                "[current-cook] resolved title from recipe stage match: '{}' ({})",
                                title, id
                            );
                            cook.recipe_title = title;
                            if cook.recipe_id.is_none() {
                                cook.recipe_id = Some(id);
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[current-cook] recipe resolution skipped: {e}");
                    }
                }
            }

            Json(cook).into_response()
        }
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            eprintln!("[current-cook] Fetch error: {e}");
            (
                StatusCode::BAD_GATEWAY,
                format!("Failed to fetch current cook: {e}"),
            )
                .into_response()
        }
    }
}
