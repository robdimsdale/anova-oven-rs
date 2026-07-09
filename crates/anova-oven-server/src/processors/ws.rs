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
use uuid::Uuid;

use crate::firestore::{self, FirebaseSession};
use crate::liveness::Liveness;
use crate::protocol;
use crate::runtime::types::{WsCommand, WsEvent};

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

fn normalize_stage_id_for_ws(id: Option<&str>) -> String {
    match id {
        Some(raw) if raw.starts_with("ios-") => raw.to_string(),
        Some(raw) if raw.starts_with("android-") => {
            format!("ios-{}", raw.trim_start_matches("android-"))
        }
        Some(raw) if !raw.is_empty() => format!("ios-{raw}"),
        _ => format!("ios-{}", Uuid::new_v4()),
    }
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
    let ws_stages: Vec<serde_json::Value> = stages
        .iter()
        .map(|stage| {
            let stage_id = normalize_stage_id_for_ws(stage.id.as_deref());

            let fan_speed = stage.fan_speed;

            let temperature_bulbs_mode = stage.temperature_bulbs_mode.as_deref().unwrap_or("dry");
            let setpoint = serde_json::json!({
                "fahrenheit": (stage.temperature_c * 1.8 + 32.0).round() as i32,
                "celsius": stage.temperature_c.round() as i32,
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

            // All stages start as "automatic"; the iOS app flips the
            // newly-active stage to "manual" via a follow-up
            // CMD_APO_UPDATE_COOK_STAGES after CMD_APO_START_STAGE. Marking a
            // stage as "manual" upfront makes the oven jump to that stage
            // immediately on start, after which CMD_APO_START_STAGE returns
            // "unauthorized".
            let mut stage_obj = serde_json::json!({
                "stepType": "stage",
                "id": stage_id,
                "title": stage.title.as_deref().unwrap_or(""),
                "description": "",
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
            });

            // Only emit `steamGenerators` when the stage actually uses steam.
            // The iOS app omits it for 0%-steam stages; including it forces a
            // steam-mode reconfiguration that the oven does not expect on e.g.
            // a dry preheat.
            if stage.steam_pct > 0.0 {
                stage_obj["steamGenerators"] = serde_json::json!({
                    "mode": "steam-percentage",
                    "steamPercentage": { "setpoint": stage.steam_pct }
                });
            }

            if let Some(duration) = stage.duration_secs {
                stage_obj["timerAdded"] = serde_json::json!(true);
                stage_obj["probeAdded"] = serde_json::json!(false);
                stage_obj["timerStartOnDetect"] = serde_json::json!(false);
                stage_obj["timer"] = serde_json::json!({ "initial": duration });
            } else if let Some(probe_target_c) = stage.probe_target_c {
                stage_obj["timerAdded"] = serde_json::json!(false);
                stage_obj["probeAdded"] = serde_json::json!(true);
                stage_obj["temperatureProbe"] = serde_json::json!({
                    "setpoint": {
                        "fahrenheit": (probe_target_c * 1.8 + 32.0).round() as i32,
                        "celsius": probe_target_c.round() as i32,
                    }
                });
            }
            // Preheat (and other stages without timer/probe) omit both
            // `timerAdded` and `probeAdded` entirely — iOS does the same.

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

#[cfg(test)]
fn update_cook_stages_command_json(
    cooker_id: &str,
    stages: &[anova_oven_api::Stage],
    active_stage_id: &str,
    request_id: &str,
) -> String {
    let ws_stages: Vec<serde_json::Value> = stages
        .iter()
        .map(|stage| {
            let stage_id = normalize_stage_id_for_ws(stage.id.as_deref());
            let transition_type = if stage_id == active_stage_id {
                "manual"
            } else {
                "automatic"
            };

            let temperature_bulbs_mode = stage.temperature_bulbs_mode.as_deref().unwrap_or("dry");
            let setpoint = serde_json::json!({
                "fahrenheit": (stage.temperature_c * 1.8 + 32.0).round() as i32,
                "celsius": stage.temperature_c.round() as i32,
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

            let mut stage_obj = serde_json::json!({
                "stepType": "stage",
                "id": stage_id,
                "title": stage.title.as_deref().unwrap_or(""),
                "type": stage.kind.as_str(),
                "userActionRequired": stage.user_action_required.unwrap_or(false),
                "temperatureBulbs": temperature_bulbs,
                "heatingElements": {
                    "top": { "on": stage.heating_element_top.unwrap_or(true) },
                    "rear": { "on": stage.heating_element_rear.unwrap_or(true) },
                    "bottom": { "on": stage.heating_element_bottom.unwrap_or(true) }
                },
                "fan": { "speed": stage.fan_speed },
                "vent": { "open": stage.vent_open.unwrap_or(false) },
                "stageTransitionType": transition_type,
            });

            if let Some(duration) = stage.duration_secs {
                stage_obj["timerAdded"] = serde_json::json!(true);
                stage_obj["timer"] = serde_json::json!({ "initial": duration });
            }

            stage_obj
        })
        .collect();

    serde_json::json!({
        "command": "CMD_APO_UPDATE_COOK_STAGES",
        "payload": {
            "type": "CMD_APO_UPDATE_COOK_STAGES",
            "id": cooker_id,
            "payload": { "stages": ws_stages }
        },
        "requestId": request_id
    })
    .to_string()
}

pub async fn run(
    mut token_source: WsTokenSource,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    evt_tx: mpsc::Sender<WsEvent>,
    read_timeout: Duration,
    liveness: Arc<Liveness>,
) {
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
        match connect_and_run(&token, &mut cmd_rx, &evt_tx, read_timeout, &liveness).await {
            Ok(()) => info!("[ws] connection closed cleanly"),
            Err(e) => warn!(error = %e, "[ws] connection error"),
        }
        liveness.set_connected(false);
        let _ = evt_tx.send(WsEvent::Disconnected).await;
        info!("[ws] reconnecting in 5s");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn connect_and_run(
    token: &str,
    cmd_rx: &mut mpsc::Receiver<WsCommand>,
    evt_tx: &mpsc::Sender<WsEvent>,
    read_timeout: Duration,
    liveness: &Liveness,
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
    let (mut sink, mut stream) = ws.split();
    // Anova's cloud has been observed to leave the TCP connection half-open
    // (no FIN/RST, no further frames) without tokio-websockets or the OS
    // surfacing an error. Without this deadline, `stream.next()` can hang
    // forever, silently freezing `state.status` at its last value while
    // downstream consumers (e.g. the pico's /status poller) keep getting
    // stale-but-200 responses with no indication anything is wrong. The
    // deadline only advances on an actual inbound frame, not on command
    // traffic, so it purely measures upstream read liveness.
    let mut last_activity = Instant::now();

    loop {
        tokio::select! {
            () = tokio::time::sleep_until(last_activity + read_timeout) => {
                warn!(
                    timeout_secs = read_timeout.as_secs(),
                    "[ws] no message from Anova within read timeout; reconnecting"
                );
                return Err("Anova websocket read timeout".into());
            }
            msg = stream.next() => {
                last_activity = Instant::now();
                match msg {
                    Some(Ok(msg)) => {
                        let raw_bytes = msg.as_payload();
                        match protocol::parse_message(raw_bytes) {
                            Ok(protocol::Event::ApoState(payload)) => {
                                // Freshness signal for `/health`: this is the
                                // frame whose absence means "stale data".
                                liveness.record_state();
                                let status = protocol::to_oven_status(&payload);
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
                                if evt_tx.send(WsEvent::ApoState(status)).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Ok(protocol::Event::ApoWifiList { cooker_id: id }) => {
                                if let Some(id) = id {
                                    info!(cooker_id = %id, "[ws] cooker id received");
                                    cooker_id = Some(id.clone());
                                    if evt_tx.send(WsEvent::CookerDiscovered { cooker_id: id }).await.is_err() {
                                        return Ok(());
                                    }
                                }
                            }
                            Ok(protocol::Event::Response { request_id, status }) => {
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
                                trace!(event = ?event, "[ws] event");
                            }
                            Err(e) => {
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
                    None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
