//! Device-side processor (local mode).
//!
//! Stands in for `wss://iot-prod.anovaculinary.io`, the endpoint the oven
//! itself connects to, so the oven can be run with no Anova cloud. It
//! emits the same `WsEvent`s as the app-side `processors::ws`, so the state
//! machine doesn't care which one is running. The device protocol is in
//! `docs/oven-cloud-emulation.md`.
//!
//! Start and stop go to the oven with the state machine's request ID as the
//! command `id`. The oven lists the IDs it has handled in
//! `processedCommandIds`, so an ID showing up in a `state` is a real ack; one
//! that hasn't within [`DeviceConfig::ack_timeout`] is reported as failed.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_rustls::rustls;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tokio_websockets::{Message, ServerBuilder, WebSocketStream};
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

use crate::liveness::Liveness;
use crate::oven_log::OvenLog;
use crate::protocol::{self, device, stages};
use crate::runtime::types::{OvenControl, WsCommand, WsEvent};
use crate::telemetry;

/// The name the oven looks up and connects to. Put in the certificate only
/// for the benefit of anyone inspecting it; the oven doesn't check.
const CLOUD_HOST: &str = "iot-prod.anovaculinary.io";

/// How long the oven gets to finish the TLS and WebSocket handshakes.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a send to the oven may take before the session is given up.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Seconds between state reports, sent with `CMD_APO_SET_REPORT_STATE_RATE`
/// on every connect. The oven's own idle default is about 10 minutes, and
/// it reports every 10s while cooking unless told otherwise; 5s keeps the
/// status page and panels close to live. (Anova's cloud asks for 2s but the
/// oven was seen reporting every 10s anyway, so check the log cadence.)
const REPORT_RATE_IDLE_SECS: u64 = 30;
const REPORT_RATE_COOKING_SECS: u64 = 5;

/// How long a start or stop may take to show up in `processedCommandIds`.
/// The oven reports within about 1.5s of a command that changes the cook.
pub const DEFAULT_ACK_TIMEOUT: Duration = Duration::from_secs(10);

type DeviceStream = WebSocketStream<TlsStream<TcpStream>>;

pub struct DeviceConfig {
    /// How often to send `CMD_APO_HEALTHCHECK`. The real cloud uses 10s.
    pub healthcheck_interval: Duration,
    /// Drop the session if no `state` arrives within this long. The oven
    /// reconnects on its own.
    pub state_timeout: Duration,
    /// IANA zone answered to the oven's `timezone` requests. `None` leaves
    /// them unanswered, which the oven tolerates.
    pub time_zone: Option<jiff::tz::TimeZone>,
    /// Fail a start or stop whose ID hasn't come back in this long.
    pub ack_timeout: Duration,
}

/// A start or stop sent to the oven and not yet seen in
/// `processedCommandIds`.
#[derive(Debug)]
struct PendingAck {
    /// The command `id`, which is also the state machine's request ID.
    id: String,
    kind: &'static str,
    deadline: Instant,
}

/// Remove and return the pending commands the oven has handled.
fn take_acked(pending: &mut Vec<PendingAck>, processed: &[String]) -> Vec<PendingAck> {
    let (acked, rest) = std::mem::take(pending)
        .into_iter()
        .partition(|p| processed.contains(&p.id));
    *pending = rest;
    acked
}

/// Remove and return the pending commands past their deadline.
fn take_expired(pending: &mut Vec<PendingAck>, now: Instant) -> Vec<PendingAck> {
    let (expired, rest) = std::mem::take(pending)
        .into_iter()
        .partition(|p| p.deadline <= now);
    *pending = rest;
    expired
}

/// Build a TLS acceptor with a fresh self-signed RSA certificate.
///
/// It must be RSA: the oven negotiated `ECDHE-RSA-AES256-GCM-SHA384`,
/// and whether it offers any ECDSA suite is unknown.
pub fn tls_acceptor() -> Result<TlsAcceptor, Box<dyn std::error::Error + Send + Sync>> {
    let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_RSA_SHA256)?;
    let cert = rcgen::CertificateParams::new(vec![CLOUD_HOST.to_string()])?.self_signed(&key)?;
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der());

    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], key_der.into())?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// The zone to answer `timezone` with: `ANOVA_TIMEZONE` if set, else the
/// system's zone when it has an IANA name.
pub fn time_zone_from_env() -> Option<jiff::tz::TimeZone> {
    match std::env::var("ANOVA_TIMEZONE") {
        Ok(name) => match jiff::tz::TimeZone::get(&name) {
            Ok(tz) => Some(tz),
            Err(e) => {
                warn!(zone = %name, error = %e, "[device] unknown ANOVA_TIMEZONE; not answering timezone requests");
                None
            }
        },
        Err(_) => {
            let tz = jiff::tz::TimeZone::system();
            if tz.iana_name().is_some() {
                Some(tz)
            } else {
                warn!("[device] no ANOVA_TIMEZONE and no IANA system zone; not answering timezone requests");
                None
            }
        }
    }
}

/// A connected oven, handed from the accept loop to the session loop.
struct Session {
    device_id: String,
    peer: SocketAddr,
    ws: DeviceStream,
    log: OvenLog,
}

pub async fn run(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    config: DeviceConfig,
    mut cmd_rx: mpsc::Receiver<WsCommand>,
    evt_tx: mpsc::Sender<WsEvent>,
    liveness: Arc<Liveness>,
) {
    let (session_tx, mut session_rx) = mpsc::channel::<Session>(4);
    tokio::spawn(accept_loop(listener, acceptor, session_tx));

    let mut current: Option<Session> = None;
    // Kept across reconnects: the oven may have handled a command just
    // before the connection dropped, and will list it in its next state.
    let mut pending: Vec<PendingAck> = Vec::new();
    let mut last_state_frame = Instant::now();
    let mut healthcheck = tokio::time::interval(config.healthcheck_interval);
    healthcheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            session = session_rx.recv() => {
                let Some(mut session) = session else { return };
                // One oven at a time: a new connection is what a reconnect
                // looks like, so it replaces the old one.
                if let Some(old) = current.take() {
                    info!(old_peer = %old.peer, new_peer = %session.peer, "[device] new connection replaces the current one");
                    if !end_session(old, &evt_tx, &liveness).await {
                        return;
                    }
                }
                info!(device_id = %session.device_id, peer = %session.peer, "[device] oven connected");
                liveness.set_connected(true);
                liveness.set_oven_online(true, None);
                if evt_tx.send(WsEvent::Connected).await.is_err()
                    || evt_tx
                        .send(WsEvent::CookerDiscovered { cooker_id: session.device_id.clone() })
                        .await
                        .is_err()
                {
                    return;
                }
                let rate = serde_json::json!({
                    "idle": REPORT_RATE_IDLE_SECS,
                    "cooking": REPORT_RATE_COOKING_SECS,
                });
                if let Err(e) = send_command(&mut session.ws, "CMD_APO_SET_REPORT_STATE_RATE", Some(rate)).await {
                    warn!(error = %e, "[device] could not set the report rate; dropping connection");
                    if !end_session(session, &evt_tx, &liveness).await {
                        return;
                    }
                    continue;
                }
                last_state_frame = Instant::now();
                healthcheck.reset();
                current = Some(session);
            }
            msg = next_message(&mut current) => {
                let keep = match msg {
                    Some(Ok(msg)) => handle_message(msg, &mut current, &mut pending, &config, &evt_tx, &liveness, &mut last_state_frame).await,
                    Some(Err(e)) => {
                        warn!(error = %e, "[device] connection error");
                        Ok(false)
                    }
                    None => {
                        info!("[device] oven closed the connection");
                        Ok(false)
                    }
                };
                match keep {
                    Ok(true) => {}
                    Ok(false) => {
                        if let Some(session) = current.take() {
                            if !end_session(session, &evt_tx, &liveness).await {
                                return;
                            }
                        }
                    }
                    Err(Closed) => return,
                }
            }
            _ = healthcheck.tick(), if current.is_some() => {
                let session = current.as_mut().expect("guarded by is_some");
                if let Err(e) = send_command(&mut session.ws, "CMD_APO_HEALTHCHECK", None).await {
                    warn!(error = %e, "[device] healthcheck send failed; dropping connection");
                    let session = current.take().expect("guarded by is_some");
                    if !end_session(session, &evt_tx, &liveness).await {
                        return;
                    }
                }
            }
            () = tokio::time::sleep_until(last_state_frame + config.state_timeout), if current.is_some() => {
                // Same watchdog as the cloud side: stale state is the failure
                // that matters, and a dead socket shows up as no states too.
                warn!(
                    timeout_secs = config.state_timeout.as_secs(),
                    "[device] no state from the oven within timeout; dropping connection"
                );
                let session = current.take().expect("guarded by is_some");
                if !end_session(session, &evt_tx, &liveness).await {
                    return;
                }
            }
            () = sleep_until_first_deadline(&pending) => {
                for expired in take_expired(&mut pending, Instant::now()) {
                    warn!(id = %expired.id, kind = expired.kind, "[device] the oven didn't ack the command in time");
                    if !send_ack(&evt_tx, expired.id, "timeout").await {
                        return;
                    }
                }
            }
            cmd = cmd_rx.recv() => {
                let (id, kind, json) = match cmd {
                    Some(WsCommand::SendStart { request_id, cook_id, recipe_id: _, stages }) => {
                        let json = start_command_json(&request_id, &cook_id, &stages);
                        (request_id, "CMD_APO_START", json)
                    }
                    Some(WsCommand::SendStop { request_id }) => {
                        let json = command_json(&request_id, "CMD_APO_STOP", None);
                        (request_id, "CMD_APO_STOP", json)
                    }
                    Some(WsCommand::Control { request_id, control }) => {
                        let (kind, payload) = control_command(control);
                        let json = command_json(&request_id, kind, payload);
                        (request_id, kind, json)
                    }
                    None => return,
                };
                let Some(session) = current.as_mut() else {
                    warn!(kind, "[device] no oven connected; command not sent");
                    if !send_ack(&evt_tx, id, "disconnected").await {
                        return;
                    }
                    continue;
                };
                info!(id = %id, kind, "[device] sending command");
                debug!(contents = %json, "[device] command");
                if let Err(e) = send_text(&mut session.ws, json).await {
                    warn!(error = %e, kind, "[device] command send failed; dropping connection");
                    let session = current.take().expect("checked above");
                    if !send_ack(&evt_tx, id, "send failed").await
                        || !end_session(session, &evt_tx, &liveness).await
                    {
                        return;
                    }
                    continue;
                }
                pending.push(PendingAck { id, kind, deadline: Instant::now() + config.ack_timeout });
            }
        }
    }
}

async fn sleep_until_first_deadline(pending: &[PendingAck]) {
    match pending.iter().map(|p| p.deadline).min() {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Report a command's outcome to the state machine. Returns `false` if it
/// has gone away.
async fn send_ack(evt_tx: &mpsc::Sender<WsEvent>, request_id: String, status: &str) -> bool {
    evt_tx
        .send(WsEvent::CommandAck {
            request_id,
            status: status.to_string(),
        })
        .await
        .is_ok()
}

/// The state machine's side of the event channel has gone away.
struct Closed;

/// Handle one frame from the oven. `Ok(false)` drops the session.
async fn handle_message(
    msg: Message,
    current: &mut Option<Session>,
    pending: &mut Vec<PendingAck>,
    config: &DeviceConfig,
    evt_tx: &mpsc::Sender<WsEvent>,
    liveness: &Liveness,
    last_state_frame: &mut Instant,
) -> Result<bool, Closed> {
    if msg.is_ping() || msg.is_pong() {
        telemetry::count_ws_frame(if msg.is_ping() { "ping" } else { "pong" });
        // tokio-websockets has already queued the pong.
        trace!(
            kind = if msg.is_ping() { "ping" } else { "pong" },
            "[device] control frame"
        );
        return Ok(true);
    }
    if msg.is_close() {
        telemetry::count_ws_frame("close");
        let reason = msg
            .as_close()
            .map(|(code, text)| format!("{code:?}: {text}"))
            .unwrap_or_else(|| "no reason".into());
        info!(reason = %reason, "[device] received close frame");
        return Ok(true);
    }
    let Some(text) = msg.as_text() else {
        telemetry::count_ws_frame("other");
        debug!(
            len = msg.as_payload().len(),
            "[device] ignoring binary frame"
        );
        return Ok(true);
    };

    match device::parse_message(text.as_bytes()) {
        Ok(device::Event::State(state)) => {
            telemetry::count_ws_frame("state");
            *last_state_frame = Instant::now();
            liveness.record_state();
            if let Some(info) = &state.system_info {
                telemetry::record_system_info(info);
            }
            let status = protocol::to_oven_status(&state);
            info!(
                mode = %status.mode,
                temp_c = format!("{:.1}", status.temperature_c),
                timer_mode = %status.timer_mode,
                active_stage_index = ?status.active_stage_index,
                pending_user_action = state.cook.as_ref().map(|c| c.stage_transition_pending_user_action),
                "[device] state"
            );
            let cook = protocol::to_oven_cook(&state);
            if let Some(session) = current.as_mut() {
                let stages = cook.as_ref().map_or(&[][..], |c| c.stages.as_slice());
                session.log.observe(&status, stages);
            }
            if evt_tx
                .send(WsEvent::ApoState { status, cook })
                .await
                .is_err()
            {
                return Err(Closed);
            }
            // After the state, so the state machine sees the oven's new
            // state before the ack that confirms it.
            for acked in take_acked(pending, &state.state.processed_command_ids) {
                info!(id = %acked.id, kind = acked.kind, "[device] oven acked the command");
                if !send_ack(evt_tx, acked.id, "ok").await {
                    return Err(Closed);
                }
            }
        }
        Ok(device::Event::Other { action }) => match action.as_str() {
            "healthcheck" => {
                telemetry::count_ws_frame("response");
                trace!("[device] healthcheck reply");
            }
            "timezone" => {
                telemetry::count_ws_frame("other");
                let Some(tz) = &config.time_zone else {
                    debug!("[device] timezone requested; no zone configured");
                    return Ok(true);
                };
                let payload = time_zone_payload(tz, jiff::Timestamp::now());
                info!(payload = %payload, "[device] answering timezone request");
                let session = current.as_mut().expect("message came from this session");
                if let Err(e) =
                    send_command(&mut session.ws, "CMD_APO_SET_TIME_ZONE", Some(payload)).await
                {
                    warn!(error = %e, "[device] timezone send failed; dropping connection");
                    return Ok(false);
                }
            }
            // `pairing` and `diagnostic` need no reply; log them in full.
            _ => {
                telemetry::count_ws_frame("other");
                info!(action = %action, message = %text, "[device] message");
            }
        },
        Err(e) => {
            telemetry::count_ws_frame("parse_error");
            warn!(error = %e, "[device] parse error");
            if evt_tx
                .send(WsEvent::ParseError {
                    detail: e.to_string(),
                })
                .await
                .is_err()
            {
                return Err(Closed);
            }
        }
    }
    Ok(true)
}

/// Close a session and report the disconnect. Returns `false` if the state
/// machine has gone away.
async fn end_session(
    session: Session,
    evt_tx: &mpsc::Sender<WsEvent>,
    liveness: &Liveness,
) -> bool {
    info!(device_id = %session.device_id, peer = %session.peer, "[device] oven disconnected");
    let mut ws = session.ws;
    let _ = tokio::time::timeout(Duration::from_secs(1), ws.close()).await;
    liveness.set_connected(false);
    // Here the oven *is* the link: say so, so a panel blames the oven.
    liveness.set_oven_online(false, None);
    metrics::counter!(telemetry::WS_RECONNECTS).increment(1);
    evt_tx.send(WsEvent::Disconnected).await.is_ok()
}

async fn next_message(
    current: &mut Option<Session>,
) -> Option<Result<Message, tokio_websockets::Error>> {
    match current {
        Some(session) => session.ws.next().await,
        None => std::future::pending().await,
    }
}

/// Send `{id, type, payload}`, the device-side command envelope, with a
/// fresh ID. For commands nobody waits on an ack for.
async fn send_command(
    ws: &mut DeviceStream,
    kind: &str,
    payload: Option<serde_json::Value>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let json = command_json(&Uuid::new_v4().to_string(), kind, payload);
    trace!(contents = %json, "[device] sending");
    send_text(ws, json).await
}

async fn send_text(
    ws: &mut DeviceStream,
    json: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tokio::time::timeout(SEND_TIMEOUT, ws.send(Message::text(json)))
        .await
        .map_err(|_| "send timed out")??;
    Ok(())
}

/// The device-side `type` and `payload` for a control.
fn control_command(control: OvenControl) -> (&'static str, Option<serde_json::Value>) {
    match control {
        OvenControl::StartStage { stage_id } => (
            "CMD_APO_START_STAGE",
            Some(serde_json::json!({ "stageId": stage_id })),
        ),
        OvenControl::SetTimer { secs } => (
            "CMD_APO_SET_TIMER",
            Some(serde_json::json!({ "initial": secs })),
        ),
        OvenControl::SetTemperatureBulbs { mode, celsius } => (
            "CMD_APO_SET_TEMPERATURE_BULBS",
            Some(stages::temperature_bulbs_payload(&mode, celsius)),
        ),
        OvenControl::SetLamp { on } => ("CMD_APO_SET_LAMP", Some(serde_json::json!({ "on": on }))),
        OvenControl::SetTemperatureUnit { unit } => (
            "CMD_APO_SET_TEMPERATURE_UNIT",
            Some(serde_json::json!({ "temperatureUnit": unit })),
        ),
        OvenControl::UpdateCookStages { stages } => (
            "CMD_APO_UPDATE_COOK_STAGES",
            Some(serde_json::json!({ "stages": stages })),
        ),
    }
}

fn start_command_json(id: &str, cook_id: &str, stages: &[anova_oven_api::Stage]) -> String {
    command_json(
        id,
        "CMD_APO_START",
        Some(serde_json::json!({
            "cookId": cook_id,
            "stages": stages::start_stages_json(stages),
        })),
    )
}

fn command_json(id: &str, kind: &str, payload: Option<serde_json::Value>) -> String {
    let mut msg = serde_json::json!({ "id": id, "type": kind });
    if let Some(payload) = payload {
        msg["payload"] = payload;
    }
    msg.to_string()
}

/// `CMD_APO_SET_TIME_ZONE`'s payload for the zone's offset at `now`. Taking
/// the offset now, DST included, avoids the hour-off clock a fixed
/// `PST`/`-28800` gives in summer.
fn time_zone_payload(tz: &jiff::tz::TimeZone, now: jiff::Timestamp) -> serde_json::Value {
    let info = tz.to_offset_info(now);
    serde_json::json!({
        "time_zone": {
            "code": info.abbreviation(),
            "gmt_offset": info.offset().seconds(),
            "id": tz.iana_name().unwrap_or("UTC"),
        }
    })
}

/// Read `deviceId` from the oven's `Authorization` JWT. The signature isn't
/// checked: we don't have the oven's public key.
fn device_id_from_jwt(token: &str) -> Option<String> {
    let token = token.strip_prefix("Bearer ").unwrap_or(token);
    let claims = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD_INDIFFERENT
        .decode(claims)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("deviceId")?.as_str().map(String::from)
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    session_tx: mpsc::Sender<Session>,
) {
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                warn!(error = %e, "[device] accept failed");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let tx = session_tx.clone();
        tokio::spawn(async move {
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(acceptor, tcp, peer)).await {
                Ok(Ok(session)) => {
                    let _ = tx.send(session).await;
                }
                Ok(Err(e)) => warn!(peer = %peer, error = %e, "[device] handshake failed"),
                Err(_) => warn!(peer = %peer, "[device] handshake timed out"),
            }
        });
        if session_tx.is_closed() {
            return;
        }
    }
}

async fn handshake(
    acceptor: TlsAcceptor,
    tcp: TcpStream,
    peer: SocketAddr,
) -> Result<Session, Box<dyn std::error::Error + Send + Sync>> {
    let tls = acceptor.accept(tcp).await?;
    let (request, ws) = ServerBuilder::new().accept(tls).await?;
    let device_id = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(device_id_from_jwt);
    let device_id = match device_id {
        Some(id) => id,
        None => {
            warn!(peer = %peer, "[device] no deviceId in the Authorization header; accepting anyway");
            "unknown".to_string()
        }
    };
    Ok(Session {
        device_id,
        peer,
        ws,
        log: OvenLog::default(),
    })
}

#[cfg(test)]
mod tests;
