#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::string::String;
use core::fmt;
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
            // The payload structure varies; try to extract cookerId if present.
            let v: serde_json::Value = serde_json::from_slice(data)?;
            let cooker_id = v["payload"]["cookerId"]
                .as_str()
                .map(String::from);
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

// --- Public event types ---

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
    pub system_info: Option<SystemInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateInfo {
    pub mode: String,
    pub temperature_unit: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    pub firmware_version: String,
    pub hardware_version: Option<String>,
    pub online: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Nodes {
    pub door: Door,
    pub fan: Fan,
    pub heating_elements: HeatingElements,
    pub steam_generators: SteamGenerators,
    pub temperature_bulbs: TemperatureBulbs,
    pub temperature_probe: TemperatureProbe,
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
#[serde(rename_all = "camelCase")]
pub struct HeatingElements {
    pub bottom: HeatingElement,
    pub rear: HeatingElement,
    pub top: HeatingElement,
}

#[derive(Debug, Deserialize)]
pub struct HeatingElement {
    pub on: bool,
    #[serde(default)]
    pub watts: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SteamGenerators {
    pub mode: String,
    pub relative_humidity: RelativeHumidity,
}

#[derive(Debug, Deserialize)]
pub struct RelativeHumidity {
    pub current: f64,
}

#[derive(Debug, Deserialize)]
pub struct TemperatureBulbs {
    pub dry: DryBulb,
    pub wet: Option<WetBulb>,
    pub mode: String,
}

#[derive(Debug, Deserialize)]
pub struct DryBulb {
    pub current: Temperature,
    pub setpoint: Option<Temperature>,
}

#[derive(Debug, Deserialize)]
pub struct WetBulb {
    pub current: Temperature,
}

#[derive(Debug, Deserialize)]
pub struct Temperature {
    pub celsius: f64,
    pub fahrenheit: f64,
}

#[derive(Debug, Deserialize)]
pub struct TemperatureProbe {
    pub connected: bool,
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
pub struct WaterTank {
    pub empty: bool,
}

// --- Display helpers ---

impl fmt::Display for ApoStatePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let nodes = &self.state.nodes;
        let mode = &self.state.state.mode;
        let dry = &nodes.temperature_bulbs.dry.current;
        let timer = &nodes.timer;

        writeln!(f, "--- Oven State ({}) ---", self.cooker_id)?;
        writeln!(f, "  Mode:        {mode}")?;
        writeln!(
            f,
            "  Temperature: {:.1}\u{00b0}C / {:.1}\u{00b0}F (dry bulb)",
            dry.celsius, dry.fahrenheit
        )?;

        if let Some(sp) = &nodes.temperature_bulbs.dry.setpoint {
            writeln!(f, "  Setpoint:    {:.1}\u{00b0}C / {:.1}\u{00b0}F", sp.celsius, sp.fahrenheit)?;
        }

        if let Some(wet) = &nodes.temperature_bulbs.wet {
            writeln!(
                f,
                "  Wet bulb:    {:.1}\u{00b0}C / {:.1}\u{00b0}F",
                wet.current.celsius, wet.current.fahrenheit
            )?;
        }

        writeln!(
            f,
            "  Timer:       {:02}:{:02} / {:02}:{:02} ({})",
            timer.current / 60,
            timer.current % 60,
            timer.initial / 60,
            timer.initial % 60,
            timer.mode
        )?;

        writeln!(f, "  Fan speed:   {}", nodes.fan.speed)?;
        writeln!(f, "  Door:        {}", if nodes.door.closed { "closed" } else { "open" })?;

        let elem_status = |e: &HeatingElement| -> alloc::string::String {
            if e.watts > 0 {
                alloc::format!("{}W", e.watts)
            } else if e.on {
                alloc::string::String::from("standby")
            } else {
                alloc::string::String::from("off")
            }
        };
        writeln!(
            f,
            "  Heating:     top={}, bottom={}, rear={}",
            elem_status(&nodes.heating_elements.top),
            elem_status(&nodes.heating_elements.bottom),
            elem_status(&nodes.heating_elements.rear),
        )?;

        writeln!(
            f,
            "  Steam:       {} (humidity: {}%)",
            nodes.steam_generators.mode,
            nodes.steam_generators.relative_humidity.current
        )?;

        if nodes.temperature_probe.connected {
            writeln!(f, "  Probe:       connected")?;
        }

        writeln!(f, "  Water tank:  {}", if nodes.water_tank.empty { "EMPTY" } else { "ok" })?;
        writeln!(f, "  Vent:        {}", if nodes.vent.open { "open" } else { "closed" })?;

        if let Some(info) = &self.state.system_info {
            writeln!(f, "  Firmware:    {}", info.firmware_version)?;
            writeln!(f, "  Online:      {}", info.online)?;
        }

        Ok(())
    }
}
