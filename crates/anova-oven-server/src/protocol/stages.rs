//! Recipe stages as the oven expects them in `CMD_APO_START` and
//! `CMD_APO_UPDATE_COOK_STAGES`, and [`stage_from_json`] for the other
//! direction.
//!
//! Only the stage list lives here. The envelope around it differs by side:
//! the app-side API wraps it as `{command, payload: {type, id, payload},
//! requestId}`, the device side as `{id, type, payload}`.

use uuid::Uuid;

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

fn temperature_bulbs_json(stage: &anova_oven_api::Stage) -> serde_json::Value {
    let temperature_bulbs_mode = stage.temperature_bulbs_mode.as_deref().unwrap_or("dry");
    let setpoint = serde_json::json!({
        "fahrenheit": (stage.temperature_c * 1.8 + 32.0).round() as i32,
        "celsius": stage.temperature_c.round() as i32,
    });

    if temperature_bulbs_mode == "wet" {
        serde_json::json!({
            "mode": "wet",
            "wet": { "setpoint": setpoint }
        })
    } else {
        serde_json::json!({
            "mode": "dry",
            "dry": { "setpoint": setpoint }
        })
    }
}

fn heating_elements_json(stage: &anova_oven_api::Stage) -> serde_json::Value {
    serde_json::json!({
        "top": { "on": stage.heating_element_top.unwrap_or(true) },
        "rear": { "on": stage.heating_element_rear.unwrap_or(true) },
        "bottom": { "on": stage.heating_element_bottom.unwrap_or(true) }
    })
}

/// A `{celsius, fahrenheit}` setpoint in Celsius. The oven keeps whole
/// degrees of each, so a setpoint made in Fahrenheit (400 °F) comes back as
/// 204 °C, which reads as 399 °F. When the Fahrenheit value agrees with the
/// Celsius one to within a degree, it's the more precise of the two.
fn setpoint_celsius(setpoint: &serde_json::Value) -> Option<f64> {
    let celsius = setpoint.get("celsius")?.as_f64()?;
    let from_f = setpoint
        .get("fahrenheit")
        .and_then(|f| f.as_f64())
        .map(|f| ((f - 32.0) / 1.8 * 10.0).round() / 10.0)
        .filter(|c| (c - celsius).abs() <= 1.0);
    Some(from_f.unwrap_or(celsius))
}

/// Parse one stage, as stored in a Firestore recipe or cook and as the oven
/// reports it in `cook.stages`, into [`anova_oven_api::Stage`]. Missing
/// fields fall back to defaults.
pub fn stage_from_json(s: &serde_json::Value) -> anova_oven_api::Stage {
    let id = s
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .map(String::from);

    let kind = s
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("cook")
        .to_string();

    let temperature_bulbs_mode = s
        .get("temperatureBulbs")
        .and_then(|tb| tb.get("mode"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let temperature_c = s
        .get("temperatureBulbs")
        .and_then(|tb| {
            let mode = tb.get("mode")?.as_str()?;
            setpoint_celsius(tb.get(mode)?.get("setpoint")?)
        })
        .unwrap_or(0.0) as f32;

    let duration_secs = s
        .get("timer")
        .and_then(|t| t.get("initial"))
        .and_then(|t| t.as_u64());

    let timer_added = s.get("timerAdded").and_then(|v| v.as_bool());

    let probe_added = s.get("probeAdded").and_then(|v| v.as_bool());

    let probe_target_c = s
        .get("temperatureProbe")
        .and_then(|tp| tp.get("setpoint"))
        .and_then(setpoint_celsius)
        .map(|v| v as f32);

    let steam_pct = s
        .get("steamGenerators")
        .and_then(|sg| {
            let mode = sg.get("mode")?.as_str()?;
            match mode {
                "steam-percentage" => sg.get("steamPercentage")?.get("setpoint")?.as_f64(),
                "relative-humidity" => sg.get("relativeHumidity")?.get("setpoint")?.as_f64(),
                _ => None,
            }
        })
        .unwrap_or(0.0) as f32;

    let fan_speed = s
        .get("fan")
        .and_then(|f| f.get("speed"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u8;

    let user_action_required = s.get("userActionRequired").and_then(|v| v.as_bool());

    let rack_position = s
        .get("rackPosition")
        .and_then(|v| v.as_u64())
        .map(|v| v as u8);

    let heating_element_top = s
        .get("heatingElements")
        .and_then(|he| he.get("top"))
        .and_then(|t| t.get("on"))
        .and_then(|v| v.as_bool());

    let heating_element_rear = s
        .get("heatingElements")
        .and_then(|he| he.get("rear"))
        .and_then(|r| r.get("on"))
        .and_then(|v| v.as_bool());

    let heating_element_bottom = s
        .get("heatingElements")
        .and_then(|he| he.get("bottom"))
        .and_then(|b| b.get("on"))
        .and_then(|v| v.as_bool());

    let vent_open = s
        .get("vent")
        .and_then(|v| v.get("open"))
        .and_then(|v| v.as_bool());

    let title = s
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .map(String::from);

    anova_oven_api::Stage {
        id,
        kind,
        temperature_c,
        temperature_bulbs_mode,
        duration_secs,
        timer_added,
        probe_added,
        probe_target_c,
        steam_pct,
        fan_speed,
        user_action_required,
        rack_position,
        heating_element_top,
        heating_element_rear,
        heating_element_bottom,
        vent_open,
        title,
    }
}

/// A `{celsius, fahrenheit}` setpoint, rounded to whole degrees as recipes
/// carry them.
pub fn setpoint_json(celsius: f32) -> serde_json::Value {
    serde_json::json!({
        "celsius": celsius.round() as i32,
        "fahrenheit": (celsius * 1.8 + 32.0).round() as i32,
    })
}

/// `CMD_APO_SET_TEMPERATURE_BULBS`'s payload: the setpoint for `mode`
/// (`dry` or `wet`).
pub fn temperature_bulbs_payload(mode: &str, celsius: f32) -> serde_json::Value {
    let mut payload = serde_json::json!({ "mode": mode });
    payload[mode] = serde_json::json!({ "setpoint": setpoint_json(celsius) });
    payload
}

/// A stage from the oven's `cook.stages`, ready to go back in
/// `CMD_APO_UPDATE_COOK_STAGES`. The oven keeps everything it needs except
/// `stepType` and `stageTransitionType`, which the update that was tested
/// through the probe carried, so those are added back. The `id` is kept as
/// the oven has it: the active stage is matched by it.
pub fn for_update(raw: &serde_json::Value) -> serde_json::Value {
    let mut stage = raw.clone();
    if stage.get("stepType").is_none() {
        stage["stepType"] = "stage".into();
    }
    if stage.get("stageTransitionType").is_none() {
        stage["stageTransitionType"] = "automatic".into();
    }
    stage
}

/// An oven stage with its setpoint changed, in whichever bulb mode it uses.
pub fn with_temperature(raw: &serde_json::Value, celsius: f32) -> serde_json::Value {
    let mut stage = raw.clone();
    let mode = stage["temperatureBulbs"]["mode"]
        .as_str()
        .unwrap_or("dry")
        .to_string();
    stage["temperatureBulbs"]["mode"] = mode.clone().into();
    stage["temperatureBulbs"][mode.as_str()]["setpoint"] = setpoint_json(celsius);
    stage
}

/// An oven stage with its timer set to `secs`.
pub fn with_timer(raw: &serde_json::Value, secs: u64) -> serde_json::Value {
    let mut stage = raw.clone();
    stage["timer"] = serde_json::json!({ "initial": secs });
    stage
}

/// The `stages` list of a `CMD_APO_START` payload.
pub fn start_stages_json(stages: &[anova_oven_api::Stage]) -> Vec<serde_json::Value> {
    stages
        .iter()
        .map(|stage| {
            let stage_id = normalize_stage_id_for_ws(stage.id.as_deref());

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
                "temperatureBulbs": temperature_bulbs_json(stage),
                "heatingElements": heating_elements_json(stage),
                "fan": { "speed": stage.fan_speed },
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
        .collect()
}

/// The `stages` list of a `CMD_APO_UPDATE_COOK_STAGES` payload. The stage
/// whose ID matches `active_stage_id` is marked `manual`.
#[cfg_attr(not(test), allow(dead_code))] // Used once stage advance lands (Phase 4).
pub fn update_cook_stages_json(
    stages: &[anova_oven_api::Stage],
    active_stage_id: &str,
) -> Vec<serde_json::Value> {
    stages
        .iter()
        .map(|stage| {
            let stage_id = normalize_stage_id_for_ws(stage.id.as_deref());
            let transition_type = if stage_id == active_stage_id {
                "manual"
            } else {
                "automatic"
            };

            let mut stage_obj = serde_json::json!({
                "stepType": "stage",
                "id": stage_id,
                "title": stage.title.as_deref().unwrap_or(""),
                "type": stage.kind.as_str(),
                "userActionRequired": stage.user_action_required.unwrap_or(false),
                "temperatureBulbs": temperature_bulbs_json(stage),
                "heatingElements": heating_elements_json(stage),
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
        .collect()
}

#[cfg(test)]
mod tests {
    use super::setpoint_celsius;

    #[test]
    fn a_fahrenheit_setpoint_keeps_its_precision() {
        let sp = serde_json::json!({ "celsius": 204, "fahrenheit": 400 });
        assert_eq!(setpoint_celsius(&sp), Some(204.4));
    }

    #[test]
    fn celsius_wins_when_fahrenheit_is_missing_or_disagrees() {
        assert_eq!(
            setpoint_celsius(&serde_json::json!({ "celsius": 180 })),
            Some(180.0)
        );
        let odd = serde_json::json!({ "celsius": 180, "fahrenheit": 400 });
        assert_eq!(setpoint_celsius(&odd), Some(180.0));
    }
}
