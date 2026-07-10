//! HTTP inbound processor.
//!
//! Owns axum route handling and translates HTTP requests into state-machine
//! commands via typed async channels.

use std::sync::Arc;

use anova_oven_api::CookProgress;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, trace, warn};

use crate::cook_progress::CookProgressMsg;
use crate::liveness::Liveness;
use crate::runtime::types::{SmError, StateMachineCommand};

#[derive(Clone)]
pub struct HttpState {
    pub sm_cmd_tx: mpsc::Sender<StateMachineCommand>,
    pub cook_progress_rx: watch::Receiver<Option<CookProgress>>,
    pub cook_progress_msg_tx: mpsc::Sender<CookProgressMsg>,
    pub liveness: Arc<Liveness>,
}

pub(crate) fn router(state: HttpState) -> Router {
    Router::new()
        .route("/status", routing::get(handle_status))
        .route("/recipes", routing::get(handle_recipes))
        .route("/update-recipes", routing::post(handle_update_recipes))
        .route("/history", routing::get(handle_history))
        .route("/stop", routing::post(handle_stop))
        .route("/start", routing::post(handle_start))
        .route("/current-cook", routing::get(handle_current_cook))
        .route("/health", routing::get(handle_health))
        .with_state(Arc::new(state))
}

/// Liveness probe: reports upstream connection status and how long since the
/// last oven-state frame, so an external monitor can alert on silent
/// staleness. Always returns 200 (it is itself the health check) — callers
/// inspect the JSON fields.
async fn handle_health(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    json_response(StatusCode::OK, &state.liveness.snapshot())
}

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

fn map_sm_error(err: SmError) -> Response {
    match err {
        SmError::Disconnected => text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Oven state not yet received — WebSocket may still be connecting",
        ),
        SmError::RecipeNotFound(recipe_id) => text_response(
            StatusCode::NOT_FOUND,
            format!("Recipe with ID '{recipe_id}' not found"),
        ),
        SmError::NotCooking => text_response(StatusCode::CONFLICT, "not cooking"),
        SmError::Firestore(e) => {
            text_response(StatusCode::BAD_GATEWAY, format!("Firestore error: {e}"))
        }
        SmError::Internal(e) => text_response(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn send_cmd(state: &HttpState, cmd: StateMachineCommand) -> Result<(), &'static str> {
    state
        .sm_cmd_tx
        .send(cmd)
        .await
        .map_err(|_| "State machine task not running")
}

async fn handle_status(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    trace!("[http] GET /status");
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(msg) = send_cmd(&state, StateMachineCommand::GetStatus { reply: reply_tx }).await {
        warn!("[http] GET /status -> INTERNAL_SERVER_ERROR ({msg})");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    match reply_rx.await {
        Ok(Ok(mut status)) => {
            status.cook_progress = state.cook_progress_rx.borrow().clone();
            // Attach upstream link health so a display client can tell this is
            // a cached reading served while the Anova link is down (the poll
            // still 200s either way). Same source as `GET /health`.
            status.upstream = Some(anova_oven_api::UpstreamHealth {
                connected: state.liveness.connected(),
                disconnected_secs: state.liveness.disconnected_secs(),
            });
            json_response(StatusCode::OK, &status)
        }
        Ok(Err(err)) => map_sm_error(err),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "State machine dropped reply",
        ),
    }
}

async fn handle_recipes(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    trace!("[http] GET /recipes");
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(msg) = send_cmd(&state, StateMachineCommand::GetRecipes { reply: reply_tx }).await {
        warn!("[http] GET /recipes -> INTERNAL_SERVER_ERROR ({msg})");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    match reply_rx.await {
        Ok(Ok(recipes)) => json_response(StatusCode::OK, &recipes),
        Ok(Err(err)) => map_sm_error(err),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "State machine dropped reply",
        ),
    }
}

async fn handle_update_recipes(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    debug!("[http] POST /update-recipes");
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(msg) = send_cmd(
        &state,
        StateMachineCommand::RefreshRecipes { reply: reply_tx },
    )
    .await
    {
        warn!("[http] POST /update-recipes -> INTERNAL_SERVER_ERROR ({msg})");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    match reply_rx.await {
        Ok(Ok(recipes)) => {
            info!(count = recipes.len(), "[http] POST /update-recipes -> OK");
            json_response(StatusCode::OK, &recipes)
        }
        Ok(Err(err)) => map_sm_error(err),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "State machine dropped reply",
        ),
    }
}

async fn handle_history(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    trace!("[http] GET /history");
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(msg) = send_cmd(&state, StateMachineCommand::GetHistory { reply: reply_tx }).await {
        warn!("[http] GET /history -> INTERNAL_SERVER_ERROR ({msg})");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    match reply_rx.await {
        Ok(Ok(history)) => json_response(StatusCode::OK, &history),
        Ok(Err(err)) => map_sm_error(err),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "State machine dropped reply",
        ),
    }
}

async fn handle_stop(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    debug!("[http] POST /stop");
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(msg) = send_cmd(&state, StateMachineCommand::StopCook { reply: reply_tx }).await {
        warn!("[http] POST /stop -> INTERNAL_SERVER_ERROR ({msg})");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    match reply_rx.await {
        Ok(Ok(())) => {
            info!("[http] POST /stop -> NO_CONTENT");
            empty_response(StatusCode::NO_CONTENT)
        }
        Ok(Err(err)) => map_sm_error(err),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "State machine dropped reply",
        ),
    }
}

#[derive(Serialize, Deserialize)]
struct StartRequest {
    recipe_id: String,
}

async fn handle_start(
    State(state): State<Arc<HttpState>>,
    Json(req): Json<StartRequest>,
) -> impl IntoResponse {
    debug!(recipe_id = %req.recipe_id, "[http] POST /start");

    // Best-effort lookup so we can seed cook-progress immediately on start.
    let mut progress_seed: Option<(String, Vec<anova_oven_api::Stage>)> = None;
    let (recipes_reply_tx, recipes_reply_rx) = oneshot::channel();
    if send_cmd(
        &state,
        StateMachineCommand::GetRecipes {
            reply: recipes_reply_tx,
        },
    )
    .await
    .is_ok()
    {
        match recipes_reply_rx.await {
            Ok(Ok(recipes)) => {
                if let Some(recipe) = recipes.into_iter().find(|r| r.id == req.recipe_id) {
                    let mut stages = recipe.stages;
                    crate::recipe::rewrite_preheat_stage_ids(&mut stages);
                    progress_seed = Some((recipe.title, stages));
                }
            }
            Ok(Err(err)) => {
                warn!(error = %err, "[http] failed to read recipes for progress seed");
            }
            Err(_) => {
                warn!("[http] state machine dropped recipes reply for progress seed");
            }
        }
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(msg) = send_cmd(
        &state,
        StateMachineCommand::StartCook {
            recipe_id: req.recipe_id.clone(),
            reply: reply_tx,
        },
    )
    .await
    {
        warn!("[http] POST /start -> INTERNAL_SERVER_ERROR ({msg})");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    match reply_rx.await {
        Ok(Ok(())) => {
            if let Some((recipe_title, stages)) = progress_seed {
                if state
                    .cook_progress_msg_tx
                    .send(CookProgressMsg::StartedFromRecipe {
                        recipe_title,
                        stages,
                    })
                    .await
                    .is_err()
                {
                    warn!("[http] could not seed cook-progress tracker after start");
                }
            }
            info!("[http] POST /start -> NO_CONTENT");
            empty_response(StatusCode::NO_CONTENT)
        }
        Ok(Err(err)) => map_sm_error(err),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "State machine dropped reply",
        ),
    }
}

async fn handle_current_cook(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(msg) = send_cmd(
        &state,
        StateMachineCommand::GetCurrentCook { reply: reply_tx },
    )
    .await
    {
        warn!("[http] GET /current-cook -> INTERNAL_SERVER_ERROR ({msg})");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }

    match reply_rx.await {
        Ok(Ok(Some(cook))) => json_response(StatusCode::OK, &cook),
        Ok(Ok(None)) => empty_response(StatusCode::NO_CONTENT),
        Ok(Err(err)) => map_sm_error(err),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "State machine dropped reply",
        ),
    }
}
