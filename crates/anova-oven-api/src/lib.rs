#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

/// Current state of the oven, as served by `GET /status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OvenStatus {
    /// Operating mode: `"idle"`, `"cook"`, or `"preheat"`.
    pub mode: String,
    /// Temperature unit preference: `"F"` or `"C"`.
    pub temperature_unit: String,

    /// Current dry-bulb temperature in Celsius.
    pub temperature_c: f32,
    /// Target temperature in Celsius, if a cook is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_temperature_c: Option<f32>,
    /// Temperature bulb mode: `"dry"` or `"wet"` (sous vide).
    pub temperature_bulbs_mode: String,
    /// Current top dry-bulb temperature in Celsius.
    pub dry_top_temperature_c: f32,
    /// Current bottom dry-bulb temperature in Celsius.
    pub dry_bottom_temperature_c: f32,
    /// Current wet-bulb temperature in Celsius.
    pub wet_bulb_temperature_c: f32,
    /// Current probe temperature in Celsius, if probe is connected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_temperature_c: Option<f32>,

    /// Timer elapsed, in seconds.
    pub timer_current_secs: u64,
    /// Timer total duration, in seconds.
    pub timer_total_secs: u64,
    /// Timer mode: `"idle"` or `"running"`.
    pub timer_mode: String,

    /// Current relative humidity percentage (0–100).
    pub steam_pct: f32,
    /// Steam target percentage (0–100), if a cook is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steam_target_pct: Option<f32>,
    /// Steam generator mode: `"idle"` or `"running"`.
    pub steam_generator_mode: String,
    /// Boiler temperature in Celsius.
    pub boiler_celsius: f32,
    /// Boiler wattage.
    pub boiler_watts: f32,
    /// Whether the boiler requires descaling.
    pub boiler_descale_required: bool,
    /// Evaporator temperature in Celsius.
    pub evaporator_celsius: f32,
    /// Evaporator wattage.
    pub evaporator_watts: f32,

    /// Fan speed (0–100).
    pub fan_speed: u32,

    /// Whether the top heating element is on.
    pub heating_element_top_on: bool,
    /// Top heating element wattage.
    pub heating_element_top_watts: f32,
    /// Whether the rear heating element is on.
    pub heating_element_rear_on: bool,
    /// Rear heating element wattage.
    pub heating_element_rear_watts: f32,
    /// Whether the bottom heating element is on.
    pub heating_element_bottom_on: bool,
    /// Bottom heating element wattage.
    pub heating_element_bottom_watts: f32,

    /// Whether the lamp is on.
    pub lamp_on: bool,
    /// Lamp preference setting: `"on"` or `"off"`.
    pub lamp_preference: String,

    /// Whether the vent is open.
    pub vent_open: bool,
    /// Whether the door is open.
    pub door_open: bool,
    /// Whether the water tank is empty.
    pub water_tank_empty: bool,
}

impl OvenStatus {
    /// Current temperature in Celsius, matching what the Anova app displays.
    ///
    /// In dry-bulb mode this is the dry-bulb reading; in wet-bulb (sous vide)
    /// mode this is the wet-bulb reading.
    pub fn current_temperature_c(&self) -> f32 {
        if self.temperature_bulbs_mode == "wet" {
            self.wet_bulb_temperature_c
        } else {
            self.temperature_c
        }
    }

    /// Whether the oven is actively cooking (not idle).
    pub fn is_cooking(&self) -> bool {
        self.mode != "idle"
    }

    /// Infer the human-readable phase: "Preheating", "Cooking", or "Idle".
    ///
    /// The WebSocket `state.mode` reports `"cook"` for both preheat and cook
    /// stages. During preheat, the timer is idle and hasn't started counting.
    pub fn phase(&self) -> &'static str {
        match self.mode.as_str() {
            "idle" => "Idle",
            "cook" if self.timer_mode == "idle" && self.timer_current_secs == 0 => "Preheating",
            "cook" => "Cooking",
            _ => "Unknown",
        }
    }

    /// Infer the stage `kind` value matching the current phase.
    ///
    /// Returns `"preheat"` or `"cook"` to match against [`Stage::kind`].
    pub fn stage_kind(&self) -> &'static str {
        match self.mode.as_str() {
            "cook" if self.timer_mode == "idle" && self.timer_current_secs == 0 => "preheat",
            _ => "cook",
        }
    }

    /// Timer remaining in seconds, if a timer is running.
    pub fn timer_remaining_secs(&self) -> Option<u64> {
        if self.timer_mode == "running" && self.timer_total_secs > 0 {
            Some(
                self.timer_total_secs
                    .saturating_sub(self.timer_current_secs),
            )
        } else {
            None
        }
    }
}

/// A saved recipe, as served by `GET /recipes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recipe {
    /// Firestore document ID.
    pub id: String,
    pub title: String,
    /// Number of cook stages (convenience field for list views).
    pub stage_count: usize,
    pub stages: Vec<Stage>,
}

/// A single cook stage within a recipe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    /// Stage type: `"preheat"` or `"cook"`.
    pub kind: String,
    /// Target temperature in Celsius.
    pub temperature_c: f32,
    /// Temperature bulb mode: `"dry"` or `"wet"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_bulbs_mode: Option<String>,
    /// Duration in seconds, if a timed stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// Whether a timer is active for this stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timer_added: Option<bool>,
    /// Whether a probe target is set for this stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_added: Option<bool>,
    /// Probe target temperature in Celsius, if probe-based.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_target_c: Option<f32>,
    /// Steam percentage (0–100).
    pub steam_pct: f32,
    /// Fan speed (0–100).
    pub fan_speed: u8,
    /// Whether user must manually advance to this stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_action_required: Option<bool>,
    /// Rack position (1–5), counted from bottom.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rack_position: Option<u8>,
    /// Top heating element on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heating_element_top: Option<bool>,
    /// Rear heating element on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heating_element_rear: Option<bool>,
    /// Bottom heating element on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heating_element_bottom: Option<bool>,
    /// Vent open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vent_open: Option<bool>,
    /// Stage title, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// A cook history entry, as served by `GET /history`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// Resolved recipe title, or `"[custom]"` if unknown.
    pub recipe_title: String,
    /// ISO 8601 timestamp when the cook ended.
    pub ended_at: String,
    /// Number of cook stages in the completed cook.
    pub stage_count: usize,
}

/// An in-progress cook, as served by `GET /current-cook`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentCook {
    /// Resolved recipe title, or `"[custom]"` if no recipe reference.
    pub recipe_title: String,
    /// Firestore recipe document ID, if this cook came from a recipe.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipe_id: Option<String>,
    /// ISO 8601 timestamp when the cook was created.
    pub started_at: String,
    /// Cook stages (same format as recipe stages).
    pub stages: Vec<Stage>,
    /// Number of cook stages (excludes preheat).
    pub cook_stage_count: usize,
    /// Total number of stages (including preheat).
    pub total_stage_count: usize,
}

impl CurrentCook {
    /// Display name: recipe title, or "Manual cook" for custom cooks.
    pub fn display_name(&self) -> &str {
        if self.recipe_title == "[custom]" {
            "Manual cook"
        } else {
            &self.recipe_title
        }
    }

    /// Find the stage matching the oven's current phase.
    pub fn current_stage(&self, status: &OvenStatus) -> Option<&Stage> {
        let kind = status.stage_kind();
        self.stages.iter().find(|s| s.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oven_status_round_trip() {
        let status = OvenStatus {
            mode: "cook".into(),
            temperature_unit: "F".into(),
            temperature_c: 200.0,
            target_temperature_c: Some(220.0),
            temperature_bulbs_mode: "dry".into(),
            dry_top_temperature_c: 201.0,
            dry_bottom_temperature_c: 199.0,
            wet_bulb_temperature_c: 180.0,
            probe_temperature_c: Some(65.0),
            timer_current_secs: 300,
            timer_total_secs: 3600,
            timer_mode: "running".into(),
            steam_pct: 50.0,
            steam_target_pct: Some(30.0),
            steam_generator_mode: "running".into(),
            boiler_celsius: 90.0,
            boiler_watts: 240.0,
            boiler_descale_required: false,
            evaporator_celsius: 85.0,
            evaporator_watts: 80.0,
            fan_speed: 100,
            heating_element_top_on: true,
            heating_element_top_watts: 800.0,
            heating_element_rear_on: true,
            heating_element_rear_watts: 1200.0,
            heating_element_bottom_on: false,
            heating_element_bottom_watts: 0.0,
            lamp_on: false,
            lamp_preference: "on".into(),
            vent_open: false,
            door_open: false,
            water_tank_empty: false,
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: OvenStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mode, "cook");
        assert_eq!(parsed.temperature_c, 200.0);
        assert_eq!(parsed.target_temperature_c, Some(220.0));
        assert_eq!(parsed.timer_current_secs, 300);
        assert_eq!(parsed.fan_speed, 100);
        assert!(parsed.heating_element_top_on);
        assert!(!parsed.door_open);
    }

    #[test]
    fn recipe_round_trip() {
        let recipe = Recipe {
            id: "abc123".into(),
            title: "Roast Chicken".into(),
            stage_count: 2,
            stages: vec![
                Stage {
                    kind: "preheat".into(),
                    temperature_c: 220.0,
                    temperature_bulbs_mode: Some("dry".into()),
                    duration_secs: None,
                    timer_added: None,
                    probe_added: None,
                    probe_target_c: None,
                    steam_pct: 0.0,
                    fan_speed: 100,
                    user_action_required: None,
                    rack_position: None,
                    heating_element_top: None,
                    heating_element_rear: None,
                    heating_element_bottom: None,
                    vent_open: None,
                    title: None,
                },
                Stage {
                    kind: "cook".into(),
                    temperature_c: 190.0,
                    temperature_bulbs_mode: Some("dry".into()),
                    duration_secs: Some(3600),
                    timer_added: Some(true),
                    probe_added: None,
                    probe_target_c: None,
                    steam_pct: 30.0,
                    fan_speed: 80,
                    user_action_required: None,
                    rack_position: None,
                    heating_element_top: None,
                    heating_element_rear: None,
                    heating_element_bottom: None,
                    vent_open: None,
                    title: None,
                },
            ],
        };
        let json = serde_json::to_string(&recipe).unwrap();
        let parsed: Recipe = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "abc123");
        assert_eq!(parsed.stages.len(), 2);
        assert_eq!(parsed.stages[1].duration_secs, Some(3600));
    }

    #[test]
    fn history_entry_round_trip() {
        let entry = HistoryEntry {
            recipe_title: "Roast Chicken".into(),
            ended_at: "2024-01-01T12:00:00Z".into(),
            stage_count: 2,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.recipe_title, "Roast Chicken");
        assert_eq!(parsed.stage_count, 2);
    }
}
