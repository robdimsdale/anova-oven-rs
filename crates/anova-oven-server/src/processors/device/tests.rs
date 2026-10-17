//! A fake oven playing the device side against the processor: TLS 1.2 with
//! the suite the oven negotiated and no SNI, a JWT in `Authorization`, and a
//! `state` split across frames like the oven's first one after connecting.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderName, HeaderValue};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{DigitallySignedStruct, SignatureScheme};
use tokio_websockets::{ClientBuilder, Config, Message, WebSocketStream};

use super::{command_json, device_id_from_jwt, run, time_zone_payload, tls_acceptor, DeviceConfig};
use crate::liveness::Liveness;
use crate::runtime::types::{OvenControl, WsCommand, WsEvent};

const COOKING: &str = include_str!("../../../testdata/device-state-cooking.json");
const DEVICE_ID: &str = "0123456789abcdef01";
const WAIT: Duration = Duration::from_secs(5);

type OvenStream = WebSocketStream<TlsStream<TcpStream>>;

/// Accepts the server's throwaway certificate.
#[derive(Debug)]
struct AcceptAnything(Arc<rustls::crypto::CryptoProvider>);

impl ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

fn jwt(device_id: &str) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let header = b64.encode(br#"{"alg":"ES256","typ":"JWT"}"#);
    let claims = b64
        .encode(serde_json::json!({ "deviceId": device_id, "deviceClass": "oven_v1" }).to_string());
    format!("{header}.{claims}.c2lnbmF0dXJl")
}

/// Connect the way the oven does.
async fn connect_oven(addr: std::net::SocketAddr, device_id: &str) -> OvenStream {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider.cipher_suites =
        vec![rustls::crypto::aws_lc_rs::cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384];
    let provider = Arc::new(provider);
    let mut config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnything(provider)))
        .with_no_client_auth();
    config.enable_sni = false;

    let tcp = TcpStream::connect(addr).await.unwrap();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(
            ServerName::try_from("iot-prod.anovaculinary.io").unwrap(),
            tcp,
        )
        .await
        .unwrap();
    let (_, conn) = tls.get_ref();
    assert_eq!(
        conn.negotiated_cipher_suite().unwrap().suite(),
        rustls::CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
    );

    let (ws, _) = ClientBuilder::new()
        .uri("wss://iot-prod.anovaculinary.io/")
        .unwrap()
        // Small frames, so a `state` arrives in fragments.
        .config(Config::default().frame_size(256))
        .add_header(
            HeaderName::from_static("authorization"),
            HeaderValue::from_str(&jwt(device_id)).unwrap(),
        )
        .unwrap()
        .connect_on(tls)
        .await
        .unwrap();
    ws
}

struct Harness {
    addr: std::net::SocketAddr,
    evt_rx: mpsc::Receiver<WsEvent>,
    cmd_tx: mpsc::Sender<WsCommand>,
    liveness: Arc<Liveness>,
}

async fn start(healthcheck_interval: Duration, state_timeout: Duration) -> Harness {
    start_with_ack_timeout(healthcheck_interval, state_timeout, WAIT).await
}

async fn start_with_ack_timeout(
    healthcheck_interval: Duration,
    state_timeout: Duration,
    ack_timeout: Duration,
) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let (evt_tx, evt_rx) = mpsc::channel(32);
    let liveness = Arc::new(Liveness::new(state_timeout.as_secs()));
    let config = DeviceConfig {
        healthcheck_interval,
        state_timeout,
        time_zone: Some(jiff::tz::TimeZone::get("America/Los_Angeles").unwrap()),
        ack_timeout,
    };
    tokio::spawn(run(
        listener,
        tls_acceptor().unwrap(),
        config,
        cmd_rx,
        evt_tx,
        liveness.clone(),
    ));
    Harness {
        addr,
        evt_rx,
        cmd_tx,
        liveness,
    }
}

async fn next_event(rx: &mut mpsc::Receiver<WsEvent>) -> WsEvent {
    tokio::time::timeout(WAIT, rx.recv())
        .await
        .expect("timed out waiting for an event")
        .expect("event channel closed")
}

/// The next command the server sends, skipping healthchecks unless asked for.
async fn next_command(ws: &mut OvenStream, skip_healthchecks: bool) -> serde_json::Value {
    loop {
        let msg = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("timed out waiting for a command")
            .expect("connection closed")
            .unwrap();
        let Some(text) = msg.as_text() else { continue };
        let cmd: serde_json::Value = serde_json::from_str(text).unwrap();
        if skip_healthchecks && cmd["type"] == "CMD_APO_HEALTHCHECK" {
            continue;
        }
        return cmd;
    }
}

async fn expect_connected(rx: &mut mpsc::Receiver<WsEvent>, device_id: &str) {
    assert!(matches!(next_event(rx).await, WsEvent::Connected));
    match next_event(rx).await {
        WsEvent::CookerDiscovered { cooker_id } => assert_eq!(cooker_id, device_id),
        other => panic!("expected CookerDiscovered, got {other:?}"),
    }
}

#[tokio::test]
async fn oven_session_reports_state_and_answers_timezone() {
    let mut h = start(Duration::from_secs(60), Duration::from_secs(60)).await;
    let mut oven = connect_oven(h.addr, DEVICE_ID).await;
    expect_connected(&mut h.evt_rx, DEVICE_ID).await;
    assert!(h.liveness.snapshot().connected);

    let rate = next_command(&mut oven, true).await;
    assert_eq!(rate["type"], "CMD_APO_SET_REPORT_STATE_RATE");
    assert_eq!(
        rate["payload"],
        serde_json::json!({ "idle": 5, "cooking": 5 })
    );
    assert!(rate["id"].as_str().is_some());

    oven.send(Message::text(COOKING.to_string())).await.unwrap();
    match next_event(&mut h.evt_rx).await {
        WsEvent::ApoState { status, cook } => {
            assert_eq!(
                cook.unwrap().cook_id,
                "55589dd8-b90e-4d5e-a154-6810258f69ae"
            );
            assert_eq!(status.mode, "cook");
            assert_eq!(status.active_stage_index, Some(0));
        }
        other => panic!("expected ApoState, got {other:?}"),
    }

    oven.send(Message::text(
        r#"{"action": "timezone","data": {}}"#.to_string(),
    ))
    .await
    .unwrap();
    let tz = next_command(&mut oven, true).await;
    assert_eq!(tz["type"], "CMD_APO_SET_TIME_ZONE");
    assert_eq!(tz["payload"]["time_zone"]["id"], "America/Los_Angeles");

    // `pairing` needs no reply and mustn't break the session.
    oven.send(Message::text(
        r#"{"action": "pairing","data": {"userId": "uid"}}"#.to_string(),
    ))
    .await
    .unwrap();

    oven.close().await.unwrap();
    assert!(matches!(
        next_event(&mut h.evt_rx).await,
        WsEvent::Disconnected
    ));
    assert!(!h.liveness.snapshot().connected);
}

#[tokio::test]
async fn sends_healthchecks() {
    let mut h = start(Duration::from_millis(100), Duration::from_secs(60)).await;
    let mut oven = connect_oven(h.addr, DEVICE_ID).await;
    expect_connected(&mut h.evt_rx, DEVICE_ID).await;
    assert_eq!(
        next_command(&mut oven, false).await["type"],
        "CMD_APO_SET_REPORT_STATE_RATE"
    );
    let hc = next_command(&mut oven, false).await;
    assert_eq!(hc["type"], "CMD_APO_HEALTHCHECK");
    assert!(hc.get("payload").is_none());
}

#[tokio::test]
async fn new_connection_replaces_the_old_one() {
    let mut h = start(Duration::from_secs(60), Duration::from_secs(60)).await;
    let mut first = connect_oven(h.addr, DEVICE_ID).await;
    expect_connected(&mut h.evt_rx, DEVICE_ID).await;

    let _second = connect_oven(h.addr, "fedcba9876543210ff").await;
    assert!(matches!(
        next_event(&mut h.evt_rx).await,
        WsEvent::Disconnected
    ));
    expect_connected(&mut h.evt_rx, "fedcba9876543210ff").await;

    // The old connection is closed from the server's side.
    let closed = tokio::time::timeout(WAIT, async {
        while let Some(Ok(msg)) = first.next().await {
            if msg.is_close() {
                break;
            }
        }
    })
    .await;
    assert!(closed.is_ok());
}

#[tokio::test]
async fn drops_a_silent_oven() {
    let mut h = start(Duration::from_secs(60), Duration::from_millis(300)).await;
    let _oven = connect_oven(h.addr, DEVICE_ID).await;
    expect_connected(&mut h.evt_rx, DEVICE_ID).await;
    assert!(matches!(
        next_event(&mut h.evt_rx).await,
        WsEvent::Disconnected
    ));
}

/// The cooking fixture with `id` as the newest processed command.
fn state_acking(id: &str) -> String {
    let mut state: serde_json::Value = serde_json::from_str(COOKING).unwrap();
    state["data"]["state"]["processedCommandIds"]
        .as_array_mut()
        .unwrap()
        .insert(0, id.into());
    state.to_string()
}

async fn next_ack(rx: &mut mpsc::Receiver<WsEvent>) -> (String, String) {
    loop {
        match next_event(rx).await {
            WsEvent::CommandAck { request_id, status } => return (request_id, status),
            WsEvent::ApoState { .. } => continue,
            other => panic!("expected CommandAck, got {other:?}"),
        }
    }
}

fn start_cmd(request_id: &str) -> WsCommand {
    WsCommand::SendStart {
        request_id: request_id.into(),
        cook_id: "cook-1".into(),
        recipe_id: "recipe-1".into(),
        stages: vec![anova_oven_api::Stage {
            id: Some("s1".into()),
            kind: "cook".into(),
            temperature_c: 60.0,
            temperature_bulbs_mode: Some("dry".into()),
            duration_secs: Some(120),
            timer_added: Some(true),
            probe_added: Some(false),
            probe_target_c: None,
            steam_pct: 0.0,
            fan_speed: 100,
            user_action_required: Some(false),
            rack_position: Some(3),
            heating_element_top: Some(false),
            heating_element_rear: Some(true),
            heating_element_bottom: Some(false),
            vent_open: Some(false),
            title: Some("Cook".into()),
        }],
    }
}

#[tokio::test]
async fn start_and_stop_are_acked_from_processed_command_ids() {
    let mut h = start(Duration::from_secs(60), Duration::from_secs(60)).await;
    let mut oven = connect_oven(h.addr, DEVICE_ID).await;
    expect_connected(&mut h.evt_rx, DEVICE_ID).await;
    let _rate = next_command(&mut oven, true).await;

    h.cmd_tx.send(start_cmd("req-start")).await.unwrap();
    let start = next_command(&mut oven, true).await;
    assert_eq!(start["type"], "CMD_APO_START");
    assert_eq!(start["id"], "req-start");
    assert_eq!(start["payload"]["cookId"], "cook-1");
    assert_eq!(start["payload"]["stages"][0]["timer"]["initial"], 120);

    // A state that doesn't list the ID yet is no ack.
    oven.send(Message::text(COOKING.to_string())).await.unwrap();
    assert!(matches!(
        next_event(&mut h.evt_rx).await,
        WsEvent::ApoState { .. }
    ));
    oven.send(Message::text(state_acking("req-start")))
        .await
        .unwrap();
    assert_eq!(
        next_ack(&mut h.evt_rx).await,
        ("req-start".to_string(), "ok".to_string())
    );

    h.cmd_tx
        .send(WsCommand::SendStop {
            request_id: "req-stop".into(),
        })
        .await
        .unwrap();
    let stop = next_command(&mut oven, true).await;
    assert_eq!(
        stop,
        serde_json::json!({ "id": "req-stop", "type": "CMD_APO_STOP" })
    );
    oven.send(Message::text(state_acking("req-stop")))
        .await
        .unwrap();
    assert_eq!(
        next_ack(&mut h.evt_rx).await,
        ("req-stop".to_string(), "ok".to_string())
    );
}

#[tokio::test]
async fn controls_go_out_in_the_device_format_and_are_acked() {
    let mut h = start(Duration::from_secs(60), Duration::from_secs(60)).await;
    let mut oven = connect_oven(h.addr, DEVICE_ID).await;
    expect_connected(&mut h.evt_rx, DEVICE_ID).await;
    let _rate = next_command(&mut oven, true).await;

    let cases = [
        (
            OvenControl::StartStage {
                stage_id: "s2".into(),
            },
            serde_json::json!({ "id": "c1", "type": "CMD_APO_START_STAGE", "payload": { "stageId": "s2" } }),
        ),
        (
            OvenControl::SetTimer { secs: 60 },
            serde_json::json!({ "id": "c2", "type": "CMD_APO_SET_TIMER", "payload": { "initial": 60 } }),
        ),
        (
            OvenControl::SetTemperatureBulbs {
                mode: "wet".into(),
                celsius: 55.0,
            },
            serde_json::json!({ "id": "c3", "type": "CMD_APO_SET_TEMPERATURE_BULBS", "payload": {
                "mode": "wet", "wet": { "setpoint": { "celsius": 55, "fahrenheit": 131 } } } }),
        ),
        (
            OvenControl::SetLamp { on: true },
            serde_json::json!({ "id": "c4", "type": "CMD_APO_SET_LAMP", "payload": { "on": true } }),
        ),
        (
            OvenControl::SetTemperatureUnit { unit: "C".into() },
            serde_json::json!({ "id": "c5", "type": "CMD_APO_SET_TEMPERATURE_UNIT", "payload": { "temperatureUnit": "C" } }),
        ),
    ];
    for (control, expected) in cases {
        let id = expected["id"].as_str().unwrap().to_string();
        h.cmd_tx
            .send(WsCommand::Control {
                request_id: id.clone(),
                control,
            })
            .await
            .unwrap();
        assert_eq!(next_command(&mut oven, true).await, expected);
        oven.send(Message::text(state_acking(&id))).await.unwrap();
        assert_eq!(next_ack(&mut h.evt_rx).await, (id, "ok".to_string()));
    }
}

#[tokio::test]
async fn unacked_command_times_out() {
    let mut h = start_with_ack_timeout(
        Duration::from_secs(60),
        Duration::from_secs(60),
        Duration::from_millis(200),
    )
    .await;
    let mut oven = connect_oven(h.addr, DEVICE_ID).await;
    expect_connected(&mut h.evt_rx, DEVICE_ID).await;
    let _rate = next_command(&mut oven, true).await;

    h.cmd_tx.send(start_cmd("req-lost")).await.unwrap();
    assert_eq!(
        next_ack(&mut h.evt_rx).await,
        ("req-lost".to_string(), "timeout".to_string())
    );
}

#[tokio::test]
async fn command_fails_at_once_with_no_oven() {
    let mut h = start(Duration::from_secs(60), Duration::from_secs(60)).await;
    h.cmd_tx
        .send(WsCommand::SendStop {
            request_id: "req-stop".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        next_ack(&mut h.evt_rx).await,
        ("req-stop".to_string(), "disconnected".to_string())
    );
}

#[test]
fn device_id_is_read_from_the_jwt() {
    assert_eq!(
        device_id_from_jwt(&jwt(DEVICE_ID)).as_deref(),
        Some(DEVICE_ID)
    );
    assert_eq!(device_id_from_jwt("not-a-jwt"), None);
}

#[test]
fn time_zone_offset_follows_daylight_saving() {
    let tz = jiff::tz::TimeZone::get("America/Los_Angeles").unwrap();
    let summer: jiff::Timestamp = "2026-07-01T12:00:00Z".parse().unwrap();
    let winter: jiff::Timestamp = "2026-01-01T12:00:00Z".parse().unwrap();
    assert_eq!(
        time_zone_payload(&tz, summer),
        serde_json::json!({
            "time_zone": { "code": "PDT", "gmt_offset": -25200, "id": "America/Los_Angeles" }
        })
    );
    assert_eq!(
        time_zone_payload(&tz, winter)["time_zone"]["gmt_offset"],
        -28800
    );
}

#[test]
fn command_omits_payload_when_none() {
    let v: serde_json::Value =
        serde_json::from_str(&command_json("x", "CMD_APO_HEALTHCHECK", None)).unwrap();
    assert_eq!(
        v,
        serde_json::json!({ "id": "x", "type": "CMD_APO_HEALTHCHECK" })
    );
}
