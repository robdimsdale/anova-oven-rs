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
    pub temperature_unit: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Nodes {
    pub door: Door,
    pub fan: Fan,
    pub heating_elements: HeatingElements,
    pub lamp: Lamp,
    pub steam_generators: SteamGenerators,
    pub temperature_bulbs: TemperatureBulbs,
    pub temperature_probe: Option<TemperatureProbe>,
    pub timer: Timer,
    pub vent: Vent,
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
pub struct HeatingElements {
    pub bottom: HeatingElement,
    pub top: HeatingElement,
    pub rear: HeatingElement,
}

#[derive(Debug, Deserialize)]
pub struct HeatingElement {
    pub on: bool,
    pub watts: f32,
}

#[derive(Debug, Deserialize)]
pub struct Lamp {
    pub on: bool,
    pub preference: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamGenerators {
    pub relative_humidity: Option<RelativeHumidity>,
    pub mode: String,
    pub boiler: Boiler,
    pub evaporator: Evaporator,
}

#[derive(Debug, Deserialize)]
pub struct RelativeHumidity {
    pub current: f64,
    pub setpoint: Option<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Boiler {
    pub celsius: f32,
    pub watts: f32,
    pub descale_required: bool,
}

#[derive(Debug, Deserialize)]
pub struct Evaporator {
    pub celsius: f32,
    pub watts: f32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemperatureBulbs {
    pub dry: DryBulb,
    pub dry_top: SimpleBulb,
    pub dry_bottom: SimpleBulb,
    pub wet: SimpleBulb,
    pub mode: String,
}

#[derive(Debug, Deserialize)]
pub struct DryBulb {
    pub current: Temperature,
    pub setpoint: Option<Temperature>,
}

/// Used for `dryTop`, `dryBottom`, and `wet` bulbs which only expose `current`.
#[derive(Debug, Deserialize)]
pub struct SimpleBulb {
    pub current: Temperature,
}

#[derive(Debug, Deserialize)]
pub struct Temperature {
    pub celsius: f64,
}

#[derive(Debug, Deserialize)]
pub struct TemperatureProbe {
    pub connected: bool,
    pub current: Option<Temperature>,
}

#[derive(Debug, Deserialize)]
pub struct Timer {
    pub current: u64,
    pub initial: u64,
    pub mode: String,
}

#[derive(Debug, Deserialize)]
pub struct Vent {
    pub open: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaterTank {
    pub empty: bool,
}

/// Convert a parsed `ApoStatePayload` into the simplified `OvenStatus` type.
pub fn to_oven_status(payload: &ApoStatePayload) -> anova_oven_api::OvenStatus {
    let nodes = &payload.state.nodes;
    let sg = &nodes.steam_generators;
    let he = &nodes.heating_elements;
    let tb = &nodes.temperature_bulbs;
    let probe_temperature_c = nodes
        .temperature_probe
        .as_ref()
        .filter(|p| p.connected)
        .and_then(|p| p.current.as_ref())
        .map(|t| t.celsius as f32);
    anova_oven_api::OvenStatus {
        mode: payload.state.state.mode.clone(),
        temperature_unit: payload.state.state.temperature_unit.clone(),
        temperature_c: tb.dry.current.celsius as f32,
        target_temperature_c: tb.dry.setpoint.as_ref().map(|s| s.celsius as f32),
        temperature_bulbs_mode: tb.mode.clone(),
        dry_top_temperature_c: tb.dry_top.current.celsius as f32,
        dry_bottom_temperature_c: tb.dry_bottom.current.celsius as f32,
        wet_bulb_temperature_c: tb.wet.current.celsius as f32,
        probe_temperature_c,
        timer_current_secs: nodes.timer.current,
        timer_total_secs: nodes.timer.initial,
        timer_mode: nodes.timer.mode.clone(),
        steam_pct: sg
            .relative_humidity
            .as_ref()
            .map_or(0.0, |rh| rh.current as f32),
        steam_target_pct: sg
            .relative_humidity
            .as_ref()
            .and_then(|rh| rh.setpoint)
            .map(|s| s as f32),
        steam_generator_mode: sg.mode.clone(),
        boiler_celsius: sg.boiler.celsius,
        boiler_watts: sg.boiler.watts,
        boiler_descale_required: sg.boiler.descale_required,
        evaporator_celsius: sg.evaporator.celsius,
        evaporator_watts: sg.evaporator.watts,
        fan_speed: nodes.fan.speed,
        heating_element_top_on: he.top.on,
        heating_element_top_watts: he.top.watts,
        heating_element_rear_on: he.rear.on,
        heating_element_rear_watts: he.rear.watts,
        heating_element_bottom_on: he.bottom.on,
        heating_element_bottom_watts: he.bottom.watts,
        lamp_on: nodes.lamp.on,
        lamp_preference: nodes.lamp.preference.clone(),
        vent_open: nodes.vent.open,
        door_open: !nodes.door.closed,
        water_tank_empty: nodes.water_tank.empty,
    }
}
