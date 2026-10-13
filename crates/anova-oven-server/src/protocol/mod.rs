//! Anova WebSocket protocol parsing, absorbed from `anova-oven-protocol`.
//!
//! The oven's state has the same shape on both sides of Anova's cloud: the
//! app-side `EVENT_APO_STATE` carries it as `payload.state`, and the oven's own
//! `state` message carries it as `data`. Both parse into [`OvenState`], which
//! [`to_oven_status`] maps to the simplified [`anova_oven_api::OvenStatus`]
//! served by the local HTTP server.
//!
//! - [`app`] parses frames from Anova's app-side API.
//! - [`device`] parses frames the oven sends to its cloud.
//! - [`stages`] serializes recipe stages for the start and update commands,
//!   independent of either side's envelope.

use serde::Deserialize;

pub mod app;
pub mod device;
pub mod stages;

// --- Oven state ---

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OvenState {
    pub nodes: Nodes,
    pub state: StateInfo,
    /// Present when the oven is mid-cook. Carries the authoritative active
    /// stage index/id so the server doesn't need to infer them heuristically.
    pub cook: Option<Cook>,
    /// When this snapshot was taken. Advances with every report the oven
    /// sends, so a frame carrying the same value as the last one is Anova's
    /// cloud replaying its cache, not news from the oven.
    #[serde(default)]
    pub updated_timestamp: Option<String>,
    /// The oven's own view of its cloud link and versions. Metrics only; not
    /// carried into `OvenStatus`, which the panels deserialize every second.
    #[serde(default)]
    pub system_info: Option<SystemInfo>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    /// Whether Anova's cloud currently sees the oven as connected. Our own
    /// WebSocket to Anova can be healthy while this is `false`.
    pub online: Option<bool>,
    pub firmware_version: Option<String>,
    pub hardware_version: Option<String>,
    /// When the oven last dropped off Anova's cloud, as `YYYY-MM-DDTHH:MM:SSZ`.
    pub last_disconnected_timestamp: Option<String>,
}

impl SystemInfo {
    /// [`Self::last_disconnected_timestamp`] as Unix milliseconds, or `None`
    /// if absent or not in the expected format.
    pub fn last_disconnected_unix_ms(&self) -> Option<u64> {
        parse_utc_timestamp_ms(self.last_disconnected_timestamp.as_deref()?)
    }
}

/// Parse the RFC 3339 UTC timestamps Anova uses (`YYYY-MM-DDTHH:MM:SSZ`)
/// into Unix milliseconds. `None` for anything unparseable or pre-epoch.
fn parse_utc_timestamp_ms(s: &str) -> Option<u64> {
    let at = s.parse::<jiff::Timestamp>().ok()?;
    u64::try_from(at.as_millisecond()).ok()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cook {
    /// The `cookId` sent with `CMD_APO_START`, or the oven's own for a cook
    /// started from its panel.
    pub cook_id: Option<String>,
    pub active_stage_index: Option<usize>,
    pub active_stage_id: Option<String>,
    /// The oven's own copy of the running recipe, in the stage format of
    /// `CMD_APO_START` cut down to the fields it uses. Can grow mid-cook
    /// through `CMD_APO_UPDATE_COOK_STAGES`.
    #[serde(default)]
    pub stages: Vec<serde_json::Value>,
    /// Set when a stage is done and the next one waits for the user, and also
    /// by `START_NEXT_STAGE` on the last stage while its timer still runs.
    #[serde(default)]
    pub stage_transition_pending_user_action: bool,
    /// Seconds since the cook started.
    pub seconds_elapsed: Option<u64>,
    /// Seconds since the active stage started.
    #[allow(dead_code)]
    pub active_stage_seconds_elapsed: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateInfo {
    pub mode: String,
    pub temperature_unit: String,
    /// The last 10 command IDs the oven has handled, newest first.
    /// Healthchecks count too.
    #[serde(default)]
    pub processed_command_ids: Vec<String>,
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
    pub dry: SetpointBulb,
    pub dry_top: SimpleBulb,
    pub dry_bottom: SimpleBulb,
    pub wet: SetpointBulb,
    pub mode: String,
}

/// Used for `dry` and `wet`, the bulbs a stage can target. `setpoint` is
/// present only on the bulb the current stage cooks by.
#[derive(Debug, Deserialize)]
pub struct SetpointBulb {
    pub current: Temperature,
    pub setpoint: Option<Temperature>,
}

/// Used for `dryTop` and `dryBottom`, which only expose `current`.
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
    pub setpoint: Option<Temperature>,
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

/// The oven's own view of the running cook, for the store to build the
/// current cook from. `None` when no cook is running or it has no `cookId`.
pub fn to_oven_cook(oven: &OvenState) -> Option<crate::runtime::types::OvenCook> {
    let cook = oven.cook.as_ref()?;
    Some(crate::runtime::types::OvenCook {
        cook_id: cook.cook_id.clone()?,
        stages: cook.stages.iter().map(stages::stage_from_json).collect(),
        raw_stages: cook.stages.clone(),
        seconds_elapsed: cook.seconds_elapsed.unwrap_or(0),
    })
}

/// Whether the cook has finished: the last stage's timer has run out and the
/// oven is holding, waiting for a stop. All three must hold:
///
/// - `stageTransitionPendingUserAction` is set;
/// - the timer is idle (`START_NEXT_STAGE` on the last stage sets the flag
///   while the timer still runs, so the flag alone would report done early);
/// - the active stage is the last of the oven's own stage list, which
///   `UPDATE_COOK_STAGES` can grow mid-cook.
///
/// A last stage with no timer, or one ending on a probe target, never meets
/// this. The server doesn't stop the oven when it's met; neither does Anova's
/// cloud.
pub fn cook_done(oven: &OvenState) -> bool {
    let Some(cook) = &oven.cook else {
        return false;
    };
    let on_last_stage = cook
        .active_stage_index
        .is_some_and(|i| i + 1 == cook.stages.len());
    cook.stage_transition_pending_user_action && oven.nodes.timer.mode == "idle" && on_last_stage
}

/// Convert a parsed [`OvenState`] into the simplified `OvenStatus` type.
pub fn to_oven_status(oven: &OvenState) -> anova_oven_api::OvenStatus {
    let nodes = &oven.nodes;
    let sg = &nodes.steam_generators;
    let he = &nodes.heating_elements;
    let tb = &nodes.temperature_bulbs;
    let probe = nodes.temperature_probe.as_ref().filter(|p| p.connected);
    let probe_temperature_c = probe
        .and_then(|p| p.current.as_ref())
        .map(|t| t.celsius as f32);
    let probe_target_temperature_c = probe
        .and_then(|p| p.setpoint.as_ref())
        .map(|t| t.celsius as f32);
    let (active_stage_index, active_stage_id) = match &oven.cook {
        Some(cook) => (cook.active_stage_index, cook.active_stage_id.clone()),
        None => (None, None),
    };
    anova_oven_api::OvenStatus {
        mode: oven.state.mode.clone(),
        temperature_unit: oven.state.temperature_unit.clone(),
        temperature_c: tb.dry.current.celsius as f32,
        target_temperature_c: tb.dry.setpoint.as_ref().map(|s| s.celsius as f32),
        wet_target_temperature_c: tb.wet.setpoint.as_ref().map(|s| s.celsius as f32),
        temperature_bulbs_mode: tb.mode.clone(),
        dry_top_temperature_c: tb.dry_top.current.celsius as f32,
        dry_bottom_temperature_c: tb.dry_bottom.current.celsius as f32,
        wet_bulb_temperature_c: tb.wet.current.celsius as f32,
        probe_temperature_c,
        probe_target_temperature_c,
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
        active_stage_index,
        active_stage_id,
        cook_done: cook_done(oven),
        cook_id: oven.cook.as_ref().and_then(|c| c.cook_id.clone()),
        stage_transition_pending_user_action: oven
            .cook
            .as_ref()
            .is_some_and(|c| c.stage_transition_pending_user_action),
        cook_progress: None,
        // Populated by the HTTP layer from the live Liveness tracker when this
        // status is served; the protocol layer has no view of link health.
        upstream: None,
    }
}

#[cfg(test)]
mod tests {
    use super::app::{parse_message, ApoStatePayload, Event};
    use super::*;

    /// A wet-bulb stage with the probe in, trimmed from a real
    /// `EVENT_APO_STATE` (see `docs/oven-websocket-api.md`).
    fn wet_cook_frame(system_info: &str) -> String {
        format!(
            r#"{{"command":"EVENT_APO_STATE","payload":{{"cookerId":"x","type":"oven_v1","state":{{
            "nodes":{{
              "door":{{"closed":true}},"fan":{{"speed":50}},
              "heatingElements":{{"bottom":{{"on":false,"watts":0}},"rear":{{"on":true,"watts":1600}},"top":{{"on":false,"watts":0}}}},
              "lamp":{{"on":true,"preference":"on"}},
              "steamGenerators":{{"mode":"relative-humidity","relativeHumidity":{{"current":95,"setpoint":100}},
                "boiler":{{"celsius":99.5,"watts":480,"descaleRequired":false}},"evaporator":{{"celsius":60,"watts":0}}}},
              "temperatureBulbs":{{"mode":"wet",
                "dry":{{"current":{{"celsius":62.1}}}},
                "dryTop":{{"current":{{"celsius":62.3}}}},"dryBottom":{{"current":{{"celsius":61.9}}}},
                "wet":{{"current":{{"celsius":57.2}},"setpoint":{{"celsius":58.0}}}}}},
              "temperatureProbe":{{"connected":true,"current":{{"celsius":41.5}},"setpoint":{{"celsius":54.0}}}},
              "timer":{{"current":0,"initial":0,"mode":"idle"}},
              "vent":{{"open":false}},"waterTank":{{"empty":false}}
            }},
            "state":{{"mode":"cook","temperatureUnit":"C"}}{system_info}
            }}}}}}"#
        )
    }

    fn parse(frame: &str) -> ApoStatePayload {
        match parse_message(frame.as_bytes()).expect("valid frame") {
            Event::ApoState(p) => p,
            other => panic!("expected ApoState, got {other:?}"),
        }
    }

    #[test]
    fn wet_and_probe_setpoints_are_carried_into_the_status() {
        let status = to_oven_status(&parse(&wet_cook_frame("")).state);
        assert_eq!(
            status.target_temperature_c, None,
            "dry has no setpoint in wet mode"
        );
        assert_eq!(status.wet_target_temperature_c, Some(58.0));
        assert_eq!(status.probe_temperature_c, Some(41.5));
        assert_eq!(status.probe_target_temperature_c, Some(54.0));
    }

    #[test]
    fn system_info_is_optional_and_parsed_when_present() {
        assert!(parse(&wet_cook_frame("")).state.system_info.is_none());

        let payload = parse(&wet_cook_frame(
            r#","systemInfo":{"online":true,"firmwareVersion":"2.1.7","hardwareVersion":"120V Universal","powerHertz":60}"#,
        ));
        let info = payload.state.system_info.expect("systemInfo");
        assert_eq!(info.online, Some(true));
        assert_eq!(info.firmware_version.as_deref(), Some("2.1.7"));
        assert_eq!(info.hardware_version.as_deref(), Some("120V Universal"));
        assert_eq!(info.last_disconnected_unix_ms(), None);
    }

    #[test]
    fn last_disconnected_timestamp_parses_to_unix_ms() {
        let payload = parse(&wet_cook_frame(
            r#","systemInfo":{"online":false,"lastDisconnectedTimestamp":"2024-01-03T21:51:28Z"}"#,
        ));
        let info = payload.state.system_info.expect("systemInfo");
        assert_eq!(info.online, Some(false));
        // `date -u -d 2024-01-03T21:51:28Z +%s`
        assert_eq!(info.last_disconnected_unix_ms(), Some(1_704_318_688_000));
    }

    #[test]
    fn utc_timestamps_parse_across_calendar_edges() {
        assert_eq!(parse_utc_timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_utc_timestamp_ms("2024-02-29T23:59:59Z"),
            Some(1_709_251_199_000)
        );
        assert_eq!(
            parse_utc_timestamp_ms("2000-03-01T00:00:00.123Z"),
            Some(951_868_800_123)
        );
        for bad in [
            "",
            "2024-01-03",
            "2024-01-03T21:51:28",
            "2024-13-03T21:51:28Z",
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_utc_timestamp_ms(bad), None, "{bad}");
        }
    }
}
