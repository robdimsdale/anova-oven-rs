//! WebSocket inbound processor.
//!
//! Owns upstream Anova connection lifecycle and command encoding/transport.
//! It emits typed websocket events to the state machine and does not perform
//! cross-domain orchestration directly.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use http::{HeaderName, HeaderValue, Uri};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_websockets::{ClientBuilder, Message};
use tracing::{debug, info, trace, warn};

use crate::firestore::{self, FirebaseSession};
use crate::liveness::Liveness;
use crate::oven_log::OvenLog;
use crate::protocol::{self, app, stages};
use crate::runtime::types::{WsCommand, WsEvent};
use crate::telemetry;

/// Firebase ID tokens are valid for ~60 minutes. Refresh a little early so a
/// reconnect never presents an about-to-expire token to Anova's gateway.
const FIREBASE_TOKEN_TTL: Duration = Duration::from_secs(50 * 60);

/// Where the WebSocket `token` query parameter comes from.
///
/// Anova accepts either a long-lived Personal Access Token or a short-lived
/// Firebase ID token. The PAT never changes; the Firebase token must be
/// refreshed roughly hourly, which is the whole point of this type — a stale
/// token makes every reconnect attempt fail forever, silently freezing state
/// exactly like a half-open socket does.
pub enum WsTokenSource {
    /// Static PAT (`anova-eyJ…`). Long-lived; nothing to refresh.
    Pat(String),
    /// Firebase ID token derived from an auto-refreshing session. Holds its own
    /// session clone; Firebase refresh tokens are reusable, so refreshing here
    /// independently of the Firestore processor is safe.
    Firebase {
        http: reqwest::Client,
        session: FirebaseSession,
        refreshed_at: Option<Instant>,
    },
}

impl WsTokenSource {
    /// Return a currently-valid token, refreshing the Firebase session first if
    /// the previous ID token is older than [`FIREBASE_TOKEN_TTL`]. The
    /// staleness check keeps a reconnect storm during an Anova outage from
    /// hammering the token endpoint every 5s.
    async fn token(&mut self) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        match self {
            WsTokenSource::Pat(token) => Ok(token.clone()),
            WsTokenSource::Firebase {
                http,
                session,
                refreshed_at,
            } => {
                let stale = refreshed_at
                    .map(|at| at.elapsed() >= FIREBASE_TOKEN_TTL)
                    .unwrap_or(true);
                if stale {
                    firestore::refresh_session(http, session).await?;
                    *refreshed_at = Some(Instant::now());
                    info!("[ws] refreshed Firebase ID token for WebSocket auth");
                }
                Ok(session.id_token.clone())
            }
        }
    }
}

fn celcius_to_fahrenheit(c: f32) -> f32 {
    c * 9.0 / 5.0 + 32.0
}

fn stop_command_json(cooker_id: &str, request_id: &str) -> String {
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

fn start_command_json(
    cooker_id: &str,
    cook_id: &str,
    stages: &[anova_oven_api::Stage],
    request_id: &str,
) -> String {
    serde_json::json!({
        "command": "CMD_APO_START",
        "payload": {
            "type": "CMD_APO_START",
            "id": cooker_id,
            "payload": {
                "cookId": cook_id,
                "stages": stages::start_stages_json(stages)
            }
        },
        "requestId": request_id
    })
    .to_string()
}

#[cfg(test)]
fn update_cook_stages_command_json(
    cooker_id: &str,
    stages: &[anova_oven_api::Stage],
    active_stage_id: &str,
    request_id: &str,
) -> String {
    serde_json::json!({
        "command": "CMD_APO_UPDATE_COOK_STAGES",
        "payload": {
            "type": "CMD_APO_UPDATE_COOK_STAGES",
            "id": cooker_id,
            "payload": { "stages": stages::update_cook_stages_json(stages, active_stage_id) }
        },
        "requestId": request_id
    })
    .to_string()
}

pub async fn run(
    mut token_source: WsTokenSource,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    evt_tx: mpsc::Sender<WsEvent>,
    state_timeout: Duration,
    liveness: Arc<Liveness>,
) {
    // Outlives each connection: the replay a reconnect gets is recognised by
    // carrying the `updatedTimestamp` already seen on the last one.
    let mut last_updated: Option<String> = None;
    loop {
        // Obtain (and, for Firebase auth, refresh-if-stale) the token before
        // every connect so a long-lived process never dials in with an expired
        // credential after hours of uptime.
        let token = match token_source.token().await {
            Ok(token) => token,
            Err(e) => {
                warn!(error = %e, "[ws] could not obtain auth token; retrying in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        info!("[ws] connecting to Anova WebSocket");
        match connect_and_run(
            &token,
            &mut cmd_rx,
            &evt_tx,
            state_timeout,
            &liveness,
            &mut last_updated,
        )
        .await
        {
            Ok(()) => info!("[ws] connection closed cleanly"),
            Err(e) => warn!(error = %e, "[ws] connection error"),
        }
        liveness.set_connected(false);
        metrics::counter!(telemetry::WS_RECONNECTS).increment(1);
        let _ = evt_tx.send(WsEvent::Disconnected).await;
        info!("[ws] reconnecting in 5s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn connect_and_run(
    token: &str,
    cmd_rx: &mut mpsc::Receiver<WsCommand>,
    evt_tx: &mpsc::Sender<WsEvent>,
    state_timeout: Duration,
    liveness: &Liveness,
    last_updated: &mut Option<String>,
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
    liveness.set_connected(true);
    if evt_tx.send(WsEvent::Connected).await.is_err() {
        return Ok(());
    }

    let mut cooker_id: Option<String> = None;
    let mut oven_log = OvenLog::default();
    let (mut sink, mut stream) = ws.split();
    // Anova's cloud has been observed to leave the TCP connection half-open
    // (no FIN/RST, no further frames) without tokio-websockets or the OS
    // surfacing an error. Without this deadline, `stream.next()` can hang
    // forever, silently freezing `state.status` at its last value while
    // downstream consumers (e.g. the pico's /status poller) keep getting
    // stale-but-200 responses with no indication anything is wrong.
    //
    // The deadline is measured against *oven-state frames*, not any frame:
    // Anova emits EVENT_APO_STATE ~every 10 min idle / ~every 10 s cooking,
    // while keepalive pings flow more often. The failure we care about is
    // stale state, not a live-but-quiet socket — and a genuinely dead socket
    // is covered too (no frames -> no states -> we trip). `state_timeout`
    // defaults to ~2x the idle heartbeat, so a single dropped idle frame
    // won't false-trip a reconnect.
    //
    // Not every state frame is fresh, though. When the oven drops off Anova's
    // cloud, the cloud keeps answering a new connection with its cached
    // snapshot, so only frames that `classify_state` calls fresh reset the
    // deadline or count for `/health`.
    let mut last_state_frame = Instant::now();

    loop {
        tokio::select! {
            () = tokio::time::sleep_until(last_state_frame + state_timeout) => {
                warn!(
                    timeout_secs = state_timeout.as_secs(),
                    "[ws] no oven-state frame from Anova within timeout; reconnecting"
                );
                // Whatever the socket's fault, the oven hasn't been heard from:
                // the replay the next connection gets must not hide that.
                liveness.set_oven_online(false);
                return Err("Anova websocket state timeout".into());
            }
            msg = stream.next() => {
                match msg {
                    // Control frames (ping/pong/close) carry no JSON.
                    // tokio-websockets yields them here *in addition* to data
                    // frames, having already queued the auto-Pong reply. Feeding
                    // an empty ping payload to the JSON parser is what produced
                    // the recurring "EOF while parsing a value" warnings. Pings
                    // prove the socket is alive but are deliberately *not* a
                    // freshness signal — only EVENT_APO_STATE resets the
                    // deadline below.
                    Some(Ok(msg)) if msg.is_ping() || msg.is_pong() => {
                        telemetry::count_ws_frame(if msg.is_ping() { "ping" } else { "pong" });
                        trace!(kind = if msg.is_ping() { "ping" } else { "pong" }, "[ws] control frame");
                    }
                    Some(Ok(msg)) if msg.is_close() => {
                        telemetry::count_ws_frame("close");
                        let reason = msg
                            .as_close()
                            .map(|(code, text)| format!("{code:?}: {text}"))
                            .unwrap_or_else(|| "no reason".into());
                        info!(reason = %reason, "[ws] received close frame; will reconnect");
                    }
                    Some(Ok(msg)) => {
                        let raw_bytes = msg.as_payload();
                        match app::parse_message(raw_bytes) {
                            Ok(app::Event::ApoState(payload)) => {
                                telemetry::count_ws_frame("state");
                                // The freshness signal for both the reconnect
                                // watchdog and `/health`: a fresh frame is the one
                                // whose absence means "stale data".
                                match classify_state(&payload.state, last_updated.as_deref(), jiff::Timestamp::now(), state_timeout) {
                                    Freshness::Fresh => {
                                        last_state_frame = Instant::now();
                                        liveness.record_state();
                                        liveness.set_oven_online(true);
                                    }
                                    Freshness::Replay => {
                                        debug!(updated = ?payload.state.updated_timestamp, "[ws] cached state replayed");
                                    }
                                    Freshness::Offline => {
                                        warn!(
                                            updated = ?payload.state.updated_timestamp,
                                            online = ?payload.state.system_info.as_ref().and_then(|i| i.online),
                                            "[ws] oven is not reaching Anova's cloud"
                                        );
                                        liveness.set_oven_online(false);
                                    }
                                }
                                if let Some(updated) = &payload.state.updated_timestamp {
                                    *last_updated = Some(updated.clone());
                                }
                                if let Some(info) = &payload.state.system_info {
                                    telemetry::record_system_info(info);
                                }
                                let status = protocol::to_oven_status(&payload.state);
                                info!(
                                    mode = %status.mode,
                                    temp_f = format!("{:.1}", celcius_to_fahrenheit(status.temperature_c)),
                                    steam_current_pct = status.steam_pct,
                                    steam_target_pct = status.steam_target_pct.unwrap_or(0.0),
                                    probe_f = format!("{:.1}", celcius_to_fahrenheit(status.probe_temperature_c.unwrap_or(0.0))),
                                    active_stage_index = ?status.active_stage_index,
                                    active_stage_id = ?status.active_stage_id,
                                    "[ws] state"
                                );
                                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(raw_bytes) {
                                    if let Some(cook) = v.get("payload").and_then(|p| p.get("state")).and_then(|s| s.get("cook")) {
                                        debug!(cook = %cook, "[ws] state.cook");
                                    }
                                }
                                let cook = protocol::to_oven_cook(&payload.state);
                                oven_log.observe(&status, cook.as_ref().map_or(&[][..], |c| c.stages.as_slice()));
                                if evt_tx.send(WsEvent::ApoState { status, cook }).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Ok(app::Event::ApoWifiList { cooker_id: id }) => {
                                telemetry::count_ws_frame("wifi_list");
                                if let Some(id) = id {
                                    info!(cooker_id = %id, "[ws] cooker id received");
                                    cooker_id = Some(id.clone());
                                    if evt_tx.send(WsEvent::CookerDiscovered { cooker_id: id }).await.is_err() {
                                        return Ok(());
                                    }
                                }
                            }
                            Ok(app::Event::Response { request_id, status }) => {
                                telemetry::count_ws_frame("response");
                                let response_payload = serde_json::from_slice::<serde_json::Value>(raw_bytes)
                                    .ok()
                                    .and_then(|v| v.get("payload").cloned());
                                debug!(
                                    request_id = %request_id,
                                    status = %status,
                                    response_payload = ?response_payload,
                                    "[ws] response"
                                );
                                if evt_tx.send(WsEvent::CommandAck { request_id, status }).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Ok(event) => {
                                telemetry::count_ws_frame("other");
                                trace!(event = ?event, "[ws] event");
                            }
                            Err(e) => {
                                telemetry::count_ws_frame("parse_error");
                                warn!(error = %e, "[ws] parse error");
                                let _ = evt_tx.send(WsEvent::ParseError { detail: e.to_string() }).await;
                            }
                        }
                    }
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                }
            }
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(WsCommand::SendStop { request_id }) => {
                        match cooker_id.as_ref() {
                            Some(id) => {
                                let json = stop_command_json(id, &request_id);
                                debug!(contents = %json, "[ws] sending stop command");
                                sink.send(Message::text(json)).await?;
                            }
                            None => warn!("[ws] stop requested before cooker id known"),
                        }
                    }
                    Some(WsCommand::SendStart { request_id, cook_id, recipe_id: _, stages }) => {
                        match cooker_id.as_ref() {
                            Some(id) => {
                                let json = start_command_json(id, &cook_id, &stages, &request_id);
                                debug!(contents = %json, "[ws] sending start command");
                                sink.send(Message::text(json)).await?;
                            }
                            None => warn!("[ws] start requested before cooker id known"),
                        }
                    }
                    // The state machine only sends these in local mode.
                    Some(WsCommand::Control { control, .. }) => {
                        warn!(control = ?control, "[ws] oven controls aren't supported through Anova's cloud");
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

/// What a state frame says about the oven's link to Anova's cloud.
#[derive(Debug, PartialEq)]
enum Freshness {
    /// A new report from the oven.
    Fresh,
    /// The snapshot last seen, sent again by the cloud (on every connect).
    /// Says nothing either way.
    Replay,
    /// The cloud says the oven is offline, or its snapshot is older than
    /// `max_age`.
    Offline,
}

fn classify_state(
    state: &protocol::OvenState,
    last_updated: Option<&str>,
    now: jiff::Timestamp,
    max_age: Duration,
) -> Freshness {
    if state.system_info.as_ref().and_then(|i| i.online) == Some(false) {
        return Freshness::Offline;
    }
    let Some(updated) = state.updated_timestamp.as_deref() else {
        return Freshness::Fresh;
    };
    if last_updated == Some(updated) {
        return Freshness::Replay;
    }
    let too_old = updated
        .parse::<jiff::Timestamp>()
        .is_ok_and(|at| now.duration_since(at).unsigned_abs() > max_age && at < now);
    if too_old {
        Freshness::Offline
    } else {
        Freshness::Fresh
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_state, Freshness};
    use super::{start_command_json, stop_command_json, update_cook_stages_command_json};
    use anova_oven_api::Stage;

    fn sample_stage() -> Stage {
        Stage {
            id: Some("ios-stage-1".into()),
            kind: "preheat".into(),
            temperature_c: 180.0,
            temperature_bulbs_mode: Some("dry".into()),
            duration_secs: Some(600),
            timer_added: Some(true),
            probe_added: Some(false),
            probe_target_c: None,
            steam_pct: 30.0,
            fan_speed: 0,
            user_action_required: Some(false),
            rack_position: Some(3),
            heating_element_top: Some(true),
            heating_element_rear: Some(true),
            heating_element_bottom: Some(true),
            vent_open: Some(false),
            title: Some("Preheat".into()),
        }
    }

    fn idle_state(online: bool, updated: &str) -> crate::protocol::OvenState {
        let mut v: serde_json::Value =
            serde_json::from_str(include_str!("../../testdata/device-state-idle.json")).unwrap();
        v["data"]["systemInfo"]["online"] = online.into();
        v["data"]["updatedTimestamp"] = updated.into();
        serde_json::from_value(v["data"].take()).unwrap()
    }

    fn at(ts: &str) -> jiff::Timestamp {
        ts.parse().unwrap()
    }

    const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(1200);

    #[test]
    fn a_new_report_is_fresh() {
        let state = idle_state(true, "2026-10-16T20:33:24Z");
        let got = classify_state(
            &state,
            Some("2026-10-16T20:23:24Z"),
            at("2026-10-16T20:33:25Z"),
            MAX_AGE,
        );
        assert_eq!(got, Freshness::Fresh);
    }

    #[test]
    fn the_snapshot_seen_last_is_a_replay() {
        let state = idle_state(true, "2026-10-16T20:23:24Z");
        let got = classify_state(
            &state,
            Some("2026-10-16T20:23:24Z"),
            at("2026-10-16T20:25:00Z"),
            MAX_AGE,
        );
        assert_eq!(got, Freshness::Replay);
    }

    #[test]
    fn a_snapshot_older_than_max_age_means_offline() {
        // A restarted server sees the cached snapshot with nothing to compare.
        let state = idle_state(true, "2026-10-16T19:00:00Z");
        let got = classify_state(&state, None, at("2026-10-16T20:43:29Z"), MAX_AGE);
        assert_eq!(got, Freshness::Offline);
    }

    #[test]
    fn the_cloud_saying_offline_wins() {
        let state = idle_state(false, "2026-10-16T20:43:29Z");
        let got = classify_state(&state, None, at("2026-10-16T20:43:29Z"), MAX_AGE);
        assert_eq!(got, Freshness::Offline);
    }

    #[test]
    fn a_clock_ahead_of_ours_is_not_stale() {
        let state = idle_state(true, "2026-10-16T21:00:00Z");
        let got = classify_state(&state, None, at("2026-10-16T20:00:00Z"), MAX_AGE);
        assert_eq!(got, Freshness::Fresh);
    }

    #[test]
    fn stop_payload_uses_provided_request_id() {
        let json = stop_command_json("cooker", "req-1");
        assert!(json.contains("\"requestId\":\"req-1\""));
    }

    #[test]
    fn start_payload_uses_provided_request_id() {
        let json = start_command_json("cooker", "cook", &[sample_stage()], "req-2");
        assert!(json.contains("\"requestId\":\"req-2\""));
        assert!(json.contains("\"CMD_APO_START\""));
    }

    #[test]
    fn update_cook_stages_flips_active_stage_to_manual() {
        let mut active = sample_stage();
        active.id = Some("ios-active".into());
        let mut other = sample_stage();
        other.id = Some("ios-other".into());

        let json =
            update_cook_stages_command_json("cooker", &[other, active], "ios-active", "req-4");
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let stages = v["payload"]["payload"]["stages"].as_array().unwrap();
        assert_eq!(stages[0]["stageTransitionType"], "automatic");
        assert_eq!(stages[1]["stageTransitionType"], "manual");
        assert_eq!(v["command"], "CMD_APO_UPDATE_COOK_STAGES");
    }
}
