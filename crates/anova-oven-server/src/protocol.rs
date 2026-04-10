//! Anova WebSocket protocol parsing, absorbed from `anova-oven-protocol`.
//!
//! Parses raw WebSocket text frames into typed [`Event`] values and maps
//! an [`ApoStatePayload`] to the simplified [`anova_oven_api::OvenStatus`]
//! served by the local HTTP server.

use serde::Deserialize;

/// Parse a raw WebSocket message into a protocol event.
pub fn parse_message(data: &[u8]) -> Result<Event, serde_json::Error> {
    let envelope: Envelope = serde_json::from_slice(data)?;
    match envelope.command.as_str() {
        "EVENT_APO_STATE" => {
            let msg: ApoStateMessage = serde_json::from_slice(data)?;
            Ok(Event::ApoState(msg.payload))
        }
        "EVENT_APO_WIFI_LIST" => {
            let v: serde_json::Value = serde_json::from_slice(data)?;
            let cooker_id = v["payload"][0]["cookerId"].as_str().map(String::from);
            Ok(Event::ApoWifiList { cooker_id })
        }
        "EVENT_USER_STATE" => Ok(Event::UserState),
        "RESPONSE" => {
            let msg: ResponseMessage = serde_json::from_slice(data)?;
            Ok(Event::Response {
                request_id: msg.request_id,
                status: msg.payload.status,
            })
        }
        _ => Ok(Event::Unknown {
            command: envelope.command,
        }),
    }
}

#[derive(Debug)]
pub enum Event {
    ApoState(ApoStatePayload),
    ApoWifiList { cooker_id: Option<String> },
    UserState,
    Response { request_id: String, status: String },
    Unknown { command: String },
}

// --- Wire format types ---

#[derive(Deserialize)]
struct Envelope {
    command: String,
}

#[derive(Deserialize)]
struct ApoStateMessage {
    payload: ApoStatePayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseMessage {
    request_id: String,
    payload: ResponsePayload,
}

#[derive(Deserialize)]
struct ResponsePayload {
    status: String,
}

// --- Oven state ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApoStatePayload {
    pub cooker_id: String,
    pub state: OvenState,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OvenState {
    pub nodes: Nodes,
    pub state: StateInfo,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateInfo {
    pub mode: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Nodes {
    pub door: Door,
    pub fan: Fan,
    pub steam_generators: SteamGenerators,
    pub temperature_bulbs: TemperatureBulbs,
    pub timer: Timer,
    pub water_tank: WaterTank,
}

#[derive(Debug, Deserialize)]
pub struct Door {
    pub closed: bool,
}

#[derive(Debug, Deserialize)]
pub struct Fan {
    pub speed: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamGenerators {
    pub relative_humidity: RelativeHumidity,
}

#[derive(Debug, Deserialize)]
pub struct RelativeHumidity {
    pub current: f64,
}

#[derive(Debug, Deserialize)]
pub struct TemperatureBulbs {
    pub dry: DryBulb,
}

#[derive(Debug, Deserialize)]
pub struct DryBulb {
    pub current: Temperature,
    pub setpoint: Option<Temperature>,
}

#[derive(Debug, Deserialize)]
pub struct Temperature {
    pub celsius: f64,
}

#[derive(Debug, Deserialize)]
pub struct Timer {
    pub current: u64,
    pub initial: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaterTank {
    pub empty: bool,
}

/// Convert a parsed `ApoStatePayload` into the simplified `OvenStatus` type.
pub fn to_oven_status(payload: &ApoStatePayload) -> anova_oven_api::OvenStatus {
    let nodes = &payload.state.nodes;
    anova_oven_api::OvenStatus {
        mode: payload.state.state.mode.clone(),
        temperature_c: nodes.temperature_bulbs.dry.current.celsius as f32,
        target_temperature_c: nodes
            .temperature_bulbs
            .dry
            .setpoint
            .as_ref()
            .map(|s| s.celsius as f32),
        timer_current_secs: nodes.timer.current,
        timer_total_secs: nodes.timer.initial,
        steam_pct: nodes.steam_generators.relative_humidity.current as f32,
        door_open: !nodes.door.closed,
        water_tank_empty: nodes.water_tank.empty,
    }
}
