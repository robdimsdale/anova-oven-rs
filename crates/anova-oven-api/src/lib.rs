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
    /// Current dry-bulb temperature in Celsius.
    pub temperature_c: f32,
    /// Target temperature in Celsius, if a cook is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_temperature_c: Option<f32>,
    /// Timer elapsed, in seconds.
    pub timer_current_secs: u64,
    /// Timer total duration, in seconds.
    pub timer_total_secs: u64,
    /// Steam percentage (0–100).
    pub steam_pct: f32,
    /// Whether the door is open.
    pub door_open: bool,
    /// Whether the water tank is empty.
    pub water_tank_empty: bool,
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
    /// Duration in seconds, if a timed stage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u64>,
    /// Steam percentage (0–100).
    pub steam_pct: f32,
    /// Fan speed (0–100).
    pub fan_speed: u8,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oven_status_round_trip() {
        let status = OvenStatus {
            mode: "idle".into(),
            temperature_c: 20.5,
            target_temperature_c: Some(220.0),
            timer_current_secs: 300,
            timer_total_secs: 3600,
            steam_pct: 50.0,
            door_open: false,
            water_tank_empty: false,
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: OvenStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.mode, "idle");
        assert_eq!(parsed.temperature_c, 20.5);
        assert_eq!(parsed.target_temperature_c, Some(220.0));
        assert_eq!(parsed.timer_current_secs, 300);
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
                    duration_secs: None,
                    steam_pct: 0.0,
                    fan_speed: 100,
                },
                Stage {
                    kind: "cook".into(),
                    temperature_c: 190.0,
                    duration_secs: Some(3600),
                    steam_pct: 30.0,
                    fan_speed: 80,
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
