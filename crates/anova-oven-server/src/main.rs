//! Local HTTP server for the Anova Precision Oven.
//!
//! Processor-based runtime:
//! - HTTP inbound -> state machine commands
//! - WebSocket inbound -> state machine events
//! - Firestore outbound -> state machine effects/events

mod cook_progress;
mod firestore;
mod liveness;
mod processors;
mod protocol;
mod read_model;
mod recipe;
mod runtime;

use std::time::Duration;

use anova_oven_api::{CookProgress, CurrentCook, OvenStatus};
use cook_progress::CookProgressTask;
use processors::firestore::FirestoreProcessor;
use processors::http::HttpState;
use processors::state_machine::StateMachineProcessor;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::runtime::types::{
    FirestoreCommand, FirestoreEvent, StateMachineCommand, StateMachineEvent, TickKind, WsCommand,
    WsEvent,
};

const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 10;
const DEFAULT_HTTP_CONNECT_TIMEOUT_SECS: u64 = 5;
const DEFAULT_CURRENT_COOK_TIMEOUT_SECS: u64 = 4;
const DEFAULT_CURRENT_COOK_RESOLUTION_TIMEOUT_SECS: u64 = 1;
const DEFAULT_CURRENT_COOK_REFRESH_INTERVAL_SECS: u64 = 60;
const DEFAULT_RECIPES_REFRESH_INTERVAL_SECS: u64 = 3600;
const DEFAULT_HISTORY_REFRESH_INTERVAL_SECS: u64 = 3600;
// Anova pushes an idle heartbeat state roughly every ~10 minutes when
// nothing is happening; give ourselves ~2x margin before declaring the
// upstream connection dead and reconnecting.
const DEFAULT_WS_READ_TIMEOUT_SECS: u64 = 1200;

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
            "anova_oven_server=info,anova_oven_server::processors::ws=debug,anova_oven_server::processors::firestore=debug",
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

/// Spawn a long-lived task under supervision. Each of these tasks is meant to
/// run for the whole process lifetime; any exit — a clean return (an upstream
/// channel closed) or a panic — leaves the server in a half-alive "zombie"
/// state where some processors keep serving stale data while a load-bearing one
/// is gone. Rather than paper over that, we log which task died and terminate
/// the process so the OS supervisor (systemd `Restart=always`, a container
/// restart policy, etc.) brings everything back from a clean slate.
fn spawn_critical(name: &'static str, fut: impl std::future::Future<Output = ()> + Send + 'static) {
    let handle = tokio::spawn(fut);
    tokio::spawn(async move {
        let reason = match handle.await {
            Ok(()) => "returned unexpectedly (an upstream channel likely closed)".to_string(),
            Err(e) if e.is_panic() => "panicked".to_string(),
            Err(e) => format!("was cancelled: {e}"),
        };
        tracing::error!(
            task = name,
            reason = %reason,
            "critical task stopped; terminating process for supervisor restart"
        );
        // Give the non-blocking tracing appender a moment to flush before the
        // abrupt exit skips its WorkerGuard drop.
        tokio::time::sleep(Duration::from_millis(250)).await;
        std::process::exit(1);
    });
}

fn spawn_tick_loop(evt_tx: mpsc::Sender<StateMachineEvent>, period: Duration, kind: TickKind) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            if evt_tx.send(StateMachineEvent::Tick(kind)).await.is_err() {
                return;
            }
        }
    });
}

#[tokio::main]
async fn main() {
    let _tracing_guard = init_tracing();

    // Optional: a static PAT for the WebSocket. When absent, the WS falls back
    // to the auto-refreshing Firebase ID token (see below), which is the more
    // hands-off choice for a long-running server.
    let anova_token = std::env::var("ANOVA_TOKEN").ok();
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

    let current_cook_timeout = env_duration_secs(
        "ANOVA_CURRENT_COOK_TIMEOUT_SECS",
        DEFAULT_CURRENT_COOK_TIMEOUT_SECS,
    );
    let current_cook_resolution_timeout = env_duration_secs(
        "ANOVA_CURRENT_COOK_RESOLUTION_TIMEOUT_SECS",
        DEFAULT_CURRENT_COOK_RESOLUTION_TIMEOUT_SECS,
    );

    let current_cook_refresh_interval = env_duration_secs(
        "ANOVA_CURRENT_COOK_REFRESH_INTERVAL_SECS",
        DEFAULT_CURRENT_COOK_REFRESH_INTERVAL_SECS,
    );
    let recipes_refresh_interval = env_duration_secs(
        "ANOVA_RECIPES_REFRESH_INTERVAL_SECS",
        DEFAULT_RECIPES_REFRESH_INTERVAL_SECS,
    );
    let history_refresh_interval = env_duration_secs(
        "ANOVA_HISTORY_REFRESH_INTERVAL_SECS",
        DEFAULT_HISTORY_REFRESH_INTERVAL_SECS,
    );
    let ws_read_timeout =
        env_duration_secs("ANOVA_WS_READ_TIMEOUT_SECS", DEFAULT_WS_READ_TIMEOUT_SECS);
    let liveness = std::sync::Arc::new(liveness::Liveness::new(ws_read_timeout.as_secs()));

    let (sm_cmd_tx, sm_cmd_rx) = mpsc::channel::<StateMachineCommand>(64);
    let (sm_evt_tx, sm_evt_rx) = mpsc::channel::<StateMachineEvent>(256);

    let (ws_cmd_tx, ws_cmd_rx) = mpsc::channel::<WsCommand>(32);
    let (ws_evt_tx, mut ws_evt_rx) = mpsc::channel::<WsEvent>(128);

    let (fs_cmd_tx, fs_cmd_rx) = mpsc::channel::<FirestoreCommand>(128);
    let (fs_evt_tx, mut fs_evt_rx) = mpsc::channel::<FirestoreEvent>(128);

    let (read_model_tx, mut read_model_rx) = watch::channel(read_model::ReadModel::default());
    let (status_tx, status_rx) = watch::channel::<Option<OvenStatus>>(None);
    let (current_cook_tx, current_cook_rx) = watch::channel::<Option<CurrentCook>>(None);
    let (cook_progress_tx, cook_progress_rx) = watch::channel::<Option<CookProgress>>(None);
    let (cook_progress_msg_tx, cook_progress_msg_rx) = mpsc::channel(32);

    let sm_evt_tx_ws = sm_evt_tx.clone();
    spawn_critical("ws-event-forwarder", async move {
        while let Some(evt) = ws_evt_rx.recv().await {
            if sm_evt_tx_ws.send(StateMachineEvent::Ws(evt)).await.is_err() {
                return;
            }
        }
    });

    let sm_evt_tx_fs = sm_evt_tx.clone();
    spawn_critical("firestore-event-forwarder", async move {
        while let Some(evt) = fs_evt_rx.recv().await {
            if sm_evt_tx_fs
                .send(StateMachineEvent::Firestore(evt))
                .await
                .is_err()
            {
                return;
            }
        }
    });

    let ws_token_source = match anova_token {
        Some(pat) => {
            info!("[ws] using static PAT for WebSocket auth");
            processors::ws::WsTokenSource::Pat(pat)
        }
        None => {
            info!("[ws] no ANOVA_TOKEN set; using auto-refreshing Firebase ID token");
            processors::ws::WsTokenSource::Firebase {
                http: http.clone(),
                // The session was just minted by sign_in, so its ID token is
                // fresh — seed refreshed_at to skip a redundant refresh on the
                // very first connect.
                session: session.clone(),
                refreshed_at: Some(tokio::time::Instant::now()),
            }
        }
    };
    spawn_critical(
        "ws-processor",
        processors::ws::run(
            ws_token_source,
            ws_cmd_rx,
            ws_evt_tx,
            ws_read_timeout,
            liveness.clone(),
        ),
    );

    let firestore_processor = FirestoreProcessor::new(
        fs_cmd_rx,
        fs_evt_tx,
        http,
        session,
        current_cook_timeout,
        current_cook_resolution_timeout,
    );
    spawn_critical("firestore-processor", firestore_processor.run());

    let state_machine = StateMachineProcessor::new(
        sm_cmd_rx,
        sm_evt_rx,
        ws_cmd_tx.clone(),
        fs_cmd_tx.clone(),
        read_model_tx,
    );
    spawn_critical("state-machine", state_machine.run());

    spawn_critical("read-model-fanout", async move {
        loop {
            if read_model_rx.changed().await.is_err() {
                return;
            }

            let snapshot = read_model_rx.borrow().clone();
            let _ = status_tx.send(snapshot.status);
            let _ = current_cook_tx.send(snapshot.current_cook);
        }
    });

    let cook_progress_task = CookProgressTask::new(cook_progress_tx);
    spawn_critical(
        "cook-progress",
        cook_progress_task.run(status_rx, current_cook_rx, cook_progress_msg_rx),
    );

    // Startup preloads via typed ticks.
    let _ = sm_evt_tx
        .send(StateMachineEvent::Tick(TickKind::RecipesRefresh))
        .await;
    let _ = sm_evt_tx
        .send(StateMachineEvent::Tick(TickKind::HistoryRefresh))
        .await;

    spawn_tick_loop(
        sm_evt_tx.clone(),
        current_cook_refresh_interval,
        TickKind::CurrentCookRefresh,
    );
    spawn_tick_loop(
        sm_evt_tx.clone(),
        recipes_refresh_interval,
        TickKind::RecipesRefresh,
    );
    spawn_tick_loop(
        sm_evt_tx,
        history_refresh_interval,
        TickKind::HistoryRefresh,
    );

    let app = processors::http::router(HttpState {
        sm_cmd_tx,
        cook_progress_rx,
        cook_progress_msg_tx,
        liveness,
    });

    let addr = std::env::var("ANOVA_BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");
    info!(address = %addr, "HTTP server listening");
    axum::serve(listener, app).await.expect("Server error");
}
