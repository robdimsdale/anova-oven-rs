//! HTTP inbound processor.
//!
//! Owns axum route handling and translates HTTP requests into state-machine
//! commands via typed async channels. The JSON API lives under `/api`; the
//! rest of the paths are the web UI ([`ui`]).

use std::sync::Arc;

use anova_oven_api::CookProgress;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing;
use axum::{middleware, Json, Router};
use metrics_exporter_prometheus::PrometheusHandle;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, trace, warn};

use crate::cook_progress::CookProgressMsg;
use crate::liveness::Liveness;
use crate::processors::local_store::{RecipeDraft, RecipeError, RecipeFiles, RecipePatch};
use crate::runtime::types::{ControlRequest, SmError, StateMachineCommand};
use crate::telemetry;

mod ui;

#[derive(Clone)]
pub struct HttpState {
    pub sm_cmd_tx: mpsc::Sender<StateMachineCommand>,
    pub cook_progress_rx: watch::Receiver<Option<CookProgress>>,
    pub cook_progress_msg_tx: mpsc::Sender<CookProgressMsg>,
    pub liveness: Arc<Liveness>,
    /// The recipe files, in local mode. `None` in cloud mode, where recipes
    /// come from Anova's cloud and can't be edited here.
    pub recipe_files: Option<RecipeFiles>,
    pub metrics: PrometheusHandle,
    pub cook_timeline: Arc<telemetry::CookTimeline>,
}

pub(crate) fn router(state: HttpState) -> Router {
    let api = Router::new()
        .route("/status", routing::get(handle_status))
        .route(
            "/recipes",
            routing::get(handle_recipes).post(handle_create_recipe),
        )
        .route(
            "/recipes/{id}",
            routing::get(handle_get_recipe)
                .patch(handle_patch_recipe)
                .delete(handle_delete_recipe),
        )
        .route("/update-recipes", routing::post(handle_update_recipes))
        .route("/history", routing::get(handle_history))
        .route("/stop", routing::post(handle_stop))
        .route("/start", routing::post(handle_start))
        .route("/next-stage", routing::post(handle_next_stage))
        .route("/timer", routing::post(handle_timer))
        .route("/temperature", routing::post(handle_temperature))
        .route("/lamp", routing::post(handle_lamp))
        .route("/temperature-unit", routing::post(handle_temperature_unit))
        .route("/current-cook", routing::get(handle_current_cook))
        .route("/health", routing::get(handle_health));
    Router::new()
        .nest("/api", api)
        .merge(ui::router())
        .route("/metrics", routing::get(handle_metrics))
        .layer(middleware::from_fn(telemetry::track_http))
        .with_state(Arc::new(state))
}

/// Liveness probe: reports upstream connection status and how long since the
/// last oven-state frame, so an external monitor can alert on silent
/// staleness. Always returns 200 (it is itself the health check) — callers
/// inspect the JSON fields.
async fn handle_health(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    json_response(StatusCode::OK, &state.liveness.snapshot())
}

/// Prometheus scrape target. See `docs/monitoring.md`. Liveness gauges are
/// refreshed here, at scrape time, so they are never staler than the scrape.
/// The cook's stage start times are appended by hand after the recorder's
/// output, because they must disappear when the cook ends.
async fn handle_metrics(State(state): State<Arc<HttpState>>) -> impl IntoResponse {
    telemetry::record_liveness(&state.liveness);
    let mut body = state.metrics.render();
    state.cook_timeline.render(&mut body);
    build_response(StatusCode::OK, telemetry::CONTENT_TYPE, body.into_bytes())
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
        SmError::Conflict(e) => text_response(StatusCode::CONFLICT, e),
        SmError::Unsupported(e) => text_response(StatusCode::NOT_IMPLEMENTED, e),
        SmError::Store(e) => text_response(StatusCode::BAD_GATEWAY, format!("store error: {e}")),
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
            // a cached reading served while the Anova link is down or the oven
            // is offline (the poll still 200s either way). Same source as
            // `GET /health`.
            status.upstream = Some(anova_oven_api::UpstreamHealth {
                connected: state.liveness.connected(),
                disconnected_secs: state.liveness.disconnected_secs(),
                oven_offline_secs: state.liveness.oven_offline_secs(),
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

async fn handle_recipes(State(state): State<Arc<HttpState>>) -> Response {
    trace!("[http] GET /recipes");
    // Straight from the files in local mode, so a list fetched right after a
    // write shows it; the state machine's copy catches up on its refresh.
    if let Some(files) = &state.recipe_files {
        return match files.list() {
            Ok(recipes) => json_response(StatusCode::OK, &recipes),
            Err(e) => recipe_error(RecipeError::Store(e)),
        };
    }
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

fn recipe_error(err: RecipeError) -> Response {
    match err {
        RecipeError::NotFound => text_response(StatusCode::NOT_FOUND, "recipe not found"),
        RecipeError::Invalid(e) => text_response(StatusCode::UNPROCESSABLE_ENTITY, e),
        RecipeError::Store(e) => {
            warn!(error = %e, "[http] recipe store error");
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("store error: {e}"),
            )
        }
    }
}

/// Recipe writes in cloud mode, where recipes are Anova's.
fn recipes_read_only() -> Response {
    text_response(
        StatusCode::NOT_IMPLEMENTED,
        "recipes come from Anova's cloud in cloud mode and can't be edited here",
    )
}

/// Have the state machine re-read the recipes after a write, so `/start`,
/// the panels and Home Assistant see it. Its reply is the list from before
/// the refresh, so it isn't waited for.
async fn refresh_recipes(state: &HttpState) {
    let (reply_tx, _reply_rx) = oneshot::channel();
    if let Err(msg) = send_cmd(
        state,
        StateMachineCommand::RefreshRecipes { reply: reply_tx },
    )
    .await
    {
        warn!("[http] couldn't refresh recipes after a write ({msg})");
    }
}

async fn handle_get_recipe(
    State(state): State<Arc<HttpState>>,
    Path(id): Path<String>,
) -> Response {
    trace!(id, "[http] GET /recipes/{{id}}");
    let Some(files) = &state.recipe_files else {
        return recipes_read_only();
    };
    match files.get(&id) {
        Ok(Some(recipe)) => json_response(StatusCode::OK, &recipe),
        Ok(None) => recipe_error(RecipeError::NotFound),
        Err(e) => recipe_error(RecipeError::Store(e)),
    }
}

async fn handle_create_recipe(
    State(state): State<Arc<HttpState>>,
    Json(draft): Json<RecipeDraft>,
) -> Response {
    let Some(files) = &state.recipe_files else {
        return recipes_read_only();
    };
    match files.create(draft) {
        Ok(recipe) => {
            info!(id = %recipe.id, title = %recipe.title, "[http] POST /recipes -> CREATED");
            refresh_recipes(&state).await;
            json_response(StatusCode::CREATED, &recipe)
        }
        Err(e) => recipe_error(e),
    }
}

async fn handle_patch_recipe(
    State(state): State<Arc<HttpState>>,
    Path(id): Path<String>,
    Json(patch): Json<RecipePatch>,
) -> Response {
    let Some(files) = &state.recipe_files else {
        return recipes_read_only();
    };
    match files.update(&id, patch) {
        Ok(recipe) => {
            info!(id = %recipe.id, title = %recipe.title, "[http] PATCH /recipes/{{id}} -> OK");
            refresh_recipes(&state).await;
            json_response(StatusCode::OK, &recipe)
        }
        Err(e) => recipe_error(e),
    }
}

async fn handle_delete_recipe(
    State(state): State<Arc<HttpState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(files) = &state.recipe_files else {
        return recipes_read_only();
    };
    match files.delete(&id) {
        Ok(()) => {
            info!(id, "[http] DELETE /recipes/{{id}} -> NO_CONTENT");
            refresh_recipes(&state).await;
            empty_response(StatusCode::NO_CONTENT)
        }
        Err(e) => recipe_error(e),
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

/// `POST /start`: a saved recipe by `recipe_id`, or a manual cook from
/// `stages` (and an optional `title`).
#[derive(Serialize, Deserialize)]
struct StartRequest {
    #[serde(default)]
    recipe_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    stages: Option<Vec<anova_oven_api::Stage>>,
}

const MANUAL_COOK_TITLE: &str = "Manual cook";

/// Send a control to the state machine; `204` once it has gone to the oven.
/// Like `/start` and `/stop`, this doesn't wait for the oven's ack.
async fn control(state: &HttpState, route: &str, request: ControlRequest) -> Response {
    debug!(request = ?request, "[http] POST {route}");
    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = StateMachineCommand::Control {
        request,
        reply: reply_tx,
    };
    if let Err(msg) = send_cmd(state, cmd).await {
        warn!("[http] POST {route} -> INTERNAL_SERVER_ERROR ({msg})");
        return text_response(StatusCode::INTERNAL_SERVER_ERROR, msg);
    }
    match reply_rx.await {
        Ok(Ok(())) => {
            info!("[http] POST {route} -> NO_CONTENT");
            empty_response(StatusCode::NO_CONTENT)
        }
        Ok(Err(err)) => map_sm_error(err),
        Err(_) => text_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "State machine dropped reply",
        ),
    }
}

async fn handle_next_stage(State(state): State<Arc<HttpState>>) -> Response {
    control(&state, "/next-stage", ControlRequest::NextStage).await
}

#[derive(Deserialize)]
struct TimerRequest {
    seconds: u64,
}

async fn handle_timer(
    State(state): State<Arc<HttpState>>,
    Json(req): Json<TimerRequest>,
) -> Response {
    if req.seconds == 0 {
        return text_response(StatusCode::UNPROCESSABLE_ENTITY, "seconds must be above 0");
    }
    control(
        &state,
        "/timer",
        ControlRequest::Timer { secs: req.seconds },
    )
    .await
}

#[derive(Deserialize)]
struct TemperatureRequest {
    celsius: f32,
}

/// The oven's range: 25–250 °C dry. A wet-bulb stage tops out lower; the
/// oven clamps or refuses that itself.
const TEMPERATURE_RANGE_C: std::ops::RangeInclusive<f32> = 25.0..=250.0;

async fn handle_temperature(
    State(state): State<Arc<HttpState>>,
    Json(req): Json<TemperatureRequest>,
) -> Response {
    if !TEMPERATURE_RANGE_C.contains(&req.celsius) {
        return text_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "celsius must be within {}–{}",
                TEMPERATURE_RANGE_C.start(),
                TEMPERATURE_RANGE_C.end()
            ),
        );
    }
    let request = ControlRequest::Temperature {
        celsius: req.celsius,
    };
    control(&state, "/temperature", request).await
}

#[derive(Deserialize)]
struct LampRequest {
    on: bool,
}

async fn handle_lamp(
    State(state): State<Arc<HttpState>>,
    Json(req): Json<LampRequest>,
) -> Response {
    control(&state, "/lamp", ControlRequest::Lamp { on: req.on }).await
}

#[derive(Deserialize)]
struct TemperatureUnitRequest {
    unit: String,
}

async fn handle_temperature_unit(
    State(state): State<Arc<HttpState>>,
    Json(req): Json<TemperatureUnitRequest>,
) -> Response {
    let unit = req.unit.to_ascii_uppercase();
    if unit != "C" && unit != "F" {
        return text_response(StatusCode::UNPROCESSABLE_ENTITY, "unit must be C or F");
    }
    control(
        &state,
        "/temperature-unit",
        ControlRequest::TemperatureUnit { unit },
    )
    .await
}

async fn handle_start(
    State(state): State<Arc<HttpState>>,
    Json(req): Json<StartRequest>,
) -> Response {
    let (cmd_for, progress_seed) = match (req.recipe_id, req.stages) {
        (Some(recipe_id), _) => {
            debug!(recipe_id = %recipe_id, "[http] POST /start");
            let seed = recipe_progress_seed(&state, &recipe_id).await;
            (StartFrom::Recipe(recipe_id), seed)
        }
        (None, Some(stages)) => {
            let title = req
                .title
                .filter(|t| !t.trim().is_empty())
                .unwrap_or_else(|| MANUAL_COOK_TITLE.into());
            debug!(title = %title, stages = stages.len(), "[http] POST /start (manual)");
            // The same checks as a saved recipe.
            let checked = crate::processors::local_store::normalize(anova_oven_api::Recipe {
                id: String::new(),
                title,
                stage_count: 0,
                stages,
                source: Default::default(),
            });
            let recipe = match checked {
                Ok(recipe) => recipe,
                Err(e) => return recipe_error(e),
            };
            let mut seed_stages = recipe.stages.clone();
            crate::recipe::rewrite_preheat_stage_ids(&mut seed_stages);
            let seed = Some((recipe.title.clone(), seed_stages));
            (StartFrom::Manual(recipe.title, recipe.stages), seed)
        }
        (None, None) => {
            return text_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "give a recipe_id, or stages for a manual cook",
            )
        }
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    let cmd = match cmd_for {
        StartFrom::Recipe(recipe_id) => StateMachineCommand::StartCook {
            recipe_id,
            reply: reply_tx,
        },
        StartFrom::Manual(title, stages) => StateMachineCommand::StartManualCook {
            title,
            stages,
            reply: reply_tx,
        },
    };
    if let Err(msg) = send_cmd(&state, cmd).await {
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

enum StartFrom {
    Recipe(String),
    Manual(String, Vec<anova_oven_api::Stage>),
}

/// Best-effort lookup so cook-progress can be seeded the moment a recipe
/// starts.
async fn recipe_progress_seed(
    state: &HttpState,
    recipe_id: &str,
) -> Option<(String, Vec<anova_oven_api::Stage>)> {
    let (recipes_reply_tx, recipes_reply_rx) = oneshot::channel();
    send_cmd(
        state,
        StateMachineCommand::GetRecipes {
            reply: recipes_reply_tx,
        },
    )
    .await
    .ok()?;
    match recipes_reply_rx.await {
        Ok(Ok(recipes)) => {
            let recipe = recipes.into_iter().find(|r| r.id == recipe_id)?;
            let mut stages = recipe.stages;
            crate::recipe::rewrite_preheat_stage_ids(&mut stages);
            Some((recipe.title, stages))
        }
        Ok(Err(err)) => {
            warn!(error = %err, "[http] failed to read recipes for progress seed");
            None
        }
        Err(_) => {
            warn!("[http] state machine dropped recipes reply for progress seed");
            None
        }
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
