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
//! - `GET /status`  — current oven state
//! - `GET /recipes` — user's saved recipes
//! - `GET /history` — cook history (last 50 cooks)

mod firestore;
mod protocol;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};
use futures_util::StreamExt;
use http::{HeaderName, HeaderValue, Uri};
use tokio::sync::{watch, Mutex};
use tokio_websockets::ClientBuilder;

// ─── Shared application state ─────────────────────────────────────────────────

struct AppState {
    /// Latest oven status from the WebSocket stream.
    status_rx: watch::Receiver<Option<anova_oven_api::OvenStatus>>,
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
    let anova_token = std::env::var("ANOVA_TOKEN")
        .expect("ANOVA_TOKEN env var is required");
    let anova_email = std::env::var("ANOVA_EMAIL")
        .expect("ANOVA_EMAIL env var is required");
    let anova_password = std::env::var("ANOVA_PASSWORD")
        .expect("ANOVA_PASSWORD env var is required");

    let http = reqwest::Client::new();

    eprintln!("Signing into Firebase...");
    let session = firestore::sign_in(&http, &anova_email, &anova_password)
        .await
        .expect("Firebase sign-in failed");
    eprintln!("Signed in as UID {}", session.uid);

    let (status_tx, status_rx) = watch::channel(None::<anova_oven_api::OvenStatus>);

    let state = Arc::new(AppState {
        status_rx,
        recipes: Mutex::new(None),
        history: Mutex::new(None),
        session: Mutex::new(session),
        http,
    });

    // Spawn WebSocket background task.
    let ws_token = anova_token.clone();
    tokio::spawn(ws_task(ws_token, status_tx));

    let app = Router::new()
        .route("/status", axum::routing::get(handle_status))
        .route("/recipes", axum::routing::get(handle_recipes))
        .route("/history", axum::routing::get(handle_history))
        .with_state(state);

    let addr = std::env::var("ANOVA_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");
    eprintln!("Listening on http://{addr}");
    axum::serve(listener, app).await.expect("Server error");
}

// ─── WebSocket background task ─────────────────────────────────────────────────

async fn ws_task(
    token: String,
    status_tx: watch::Sender<Option<anova_oven_api::OvenStatus>>,
) {
    loop {
        eprintln!("[ws] Connecting to Anova WebSocket...");
        match ws_connect_and_run(&token, &status_tx).await {
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
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let uri = Uri::builder()
        .scheme("wss")
        .authority("devices.anovaculinary.io")
        .path_and_query(format!(
            "/?token={token}&supportedAccessories=APO&platform=android"
        ))
        .build()?;

    let (mut ws, _) = ClientBuilder::from_uri(uri)
        .add_header(
            HeaderName::from_static("sec-websocket-protocol"),
            HeaderValue::from_static("ANOVA_V2"),
        )?
        .connect()
        .await?;

    eprintln!("[ws] Connected.");

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let payload = msg.as_payload();

        if let Ok(protocol::Event::ApoState(state)) = protocol::parse_message(payload) {
            let status = protocol::to_oven_status(&state);
            eprintln!("[ws] State: mode={} temp={:.1}°C", status.mode, status.temperature_c);
            let _ = status_tx.send(Some(status));
        }
    }

    Ok(())
}

// ─── HTTP handlers ─────────────────────────────────────────────────────────────

async fn handle_status(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match state.status_rx.borrow().clone() {
        Some(status) => Json(status).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Oven state not yet received — WebSocket may still be connecting",
        )
            .into_response(),
    }
}

async fn handle_recipes(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
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
            (StatusCode::BAD_GATEWAY, format!("Failed to fetch recipes: {e}")).into_response()
        }
    }
}

async fn handle_history(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
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
            (StatusCode::BAD_GATEWAY, format!("Failed to fetch history: {e}")).into_response()
        }
    }
}
