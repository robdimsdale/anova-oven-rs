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
use tokio::sync::{mpsc, watch, Mutex};
use tokio_websockets::{ClientBuilder, Message};
use uuid::Uuid;
use serde::{Deserialize, Serialize};

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
            
            // Build the stage object with all fields from the recipe
            let mut stage_obj = serde_json::json!({
                "stepType": "stage",
                "id": stage_id,
                "title": stage.title.as_deref().unwrap_or(""),
                "type": stage.kind.as_str(),
                "userActionRequired": stage.user_action_required.unwrap_or(false),
                "stageTransitionType": "automatic",
                "temperatureBulbs": {
                    "mode": stage.temperature_bulbs_mode.as_deref().unwrap_or("dry"),
                    "dry": {
                        "setpoint": {
                            "celsius": stage.temperature_c,
                            "fahrenheit": stage.temperature_c * 1.8 + 32.0
                        }
                    }
                },
                "heatingElements": {
                    "top": { "on": stage.heating_element_top.unwrap_or(true) },
                    "rear": { "on": stage.heating_element_rear.unwrap_or(true) },
                    "bottom": { "on": stage.heating_element_bottom.unwrap_or(true) }
                },
                "fan": { "speed": stage.fan_speed },
                "vent": { "open": stage.vent_open.unwrap_or(false) },
                "rackPosition": stage.rack_position.unwrap_or(3),
                "steamGenerators": {
                    "mode": "steam-percentage",
                    "steamPercentage": { "setpoint": stage.steam_pct }
                }
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
                            _ => {}
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
                                eprintln!("[ws] Sending CMD_APO_START for cooker {id}");
                                sink.send(Message::text(start_command_json(id, &stages))).await?;
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
    match firestore::fetch_recipes(&state.http, &session).await {
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

    // Fetch the recipe by ID from the cached recipes
    let recipes = state.recipes.lock().await;
    let recipe = match &*recipes {
        Some(recipes) => recipes.iter().find(|r| r.id == req.recipe_id).cloned(),
        None => None,
    };
    
    drop(recipes);

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
    match firestore::fetch_history(&state.http, &session, 50).await {
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

async fn handle_current_cook(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let session = state.session.lock().await.clone();
    match firestore::fetch_current_cook(&state.http, &session).await {
        Ok(Some(cook)) => Json(cook).into_response(),
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
