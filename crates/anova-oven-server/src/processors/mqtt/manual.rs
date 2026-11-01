//! A manual cook from Home Assistant: one step, set up field by field in
//! number and select entities, then started with a button. It's the web
//! UI's manual cook with a single stage: a cook stage, optionally led by a
//! preheat with the same settings.
//!
//! The draft lives here, like the selected recipe, so every dashboard sees
//! the same one. It's in memory, so a server restart resets it.

use anova_oven_api::{Recipe, Stage};

use super::recipes;
use crate::processors::local_store::{normalize, RecipeError};

pub const TITLE: &str = "Manual cook";

pub const MODES: [&str; 2] = ["Oven (dry bulb)", "Sous vide (wet bulb)"];
/// The heating-element combinations the oven allows: any one or two, never
/// all three.
pub const ELEMENTS: [&str; 6] = [
    "Rear",
    "Top",
    "Bottom",
    "Top + rear",
    "Bottom + rear",
    "Top + bottom",
];
/// `(top, rear, bottom)` for each of `ELEMENTS`.
const ELEMENTS_ON: [(bool, bool, bool); 6] = [
    (false, true, false),
    (true, false, false),
    (false, false, true),
    (true, true, false),
    (false, true, true),
    (true, false, true),
];
pub const PREHEATS: [&str; 3] = [
    "No preheat",
    "Preheat, then wait for me",
    "Preheat, then start cooking",
];
pub const ENDS: [&str; 3] = ["Timer", "Probe", "When I say"];
pub const AT_END: [&str; 2] = ["Keep holding", "Turn the oven off"];

const NO_PREHEAT: usize = 0;
const PREHEAT_WAIT: usize = 1;
const ENDS_TIMER: usize = 0;
const ENDS_PROBE: usize = 1;
const TURN_OFF: usize = 1;

/// The manual cook as set up so far. Selects are indices into their
/// options above.
#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub temperature_c: f32,
    pub mode: usize,
    pub steam_pct: f32,
    pub elements: usize,
    pub preheat: usize,
    pub ends: usize,
    pub timer_mins: u32,
    pub probe_target_c: f32,
    pub at_end: usize,
}

impl Default for Draft {
    /// The web UI's new stage, with a timer that's ready to go.
    fn default() -> Self {
        Self {
            temperature_c: 180.0,
            mode: 0,
            steam_pct: 0.0,
            elements: 0,
            preheat: NO_PREHEAT,
            ends: ENDS_TIMER,
            timer_mins: 30,
            probe_target_c: 60.0,
            at_end: 0,
        }
    }
}

impl Draft {
    /// Set one field from its command (`manual/<field>`) and payload.
    pub fn set(&mut self, field: &str, payload: &str) -> Result<(), String> {
        let number = |lo: f64, hi: f64, what: &str| match payload.parse::<f64>() {
            Ok(v) if (lo..=hi).contains(&v) => Ok(v),
            _ => Err(format!("{what} wants {lo}–{hi}, not `{payload}`")),
        };
        let choice = |options: &[&str]| {
            options
                .iter()
                .position(|o| *o == payload)
                .ok_or_else(|| format!("no option `{payload}`"))
        };
        match field {
            "temperature" => self.temperature_c = number(25.0, 250.0, "temperature")? as f32,
            "mode" => self.mode = choice(&MODES)?,
            "steam" => self.steam_pct = number(0.0, 100.0, "steam")? as f32,
            "elements" => self.elements = choice(&ELEMENTS)?,
            "preheat" => self.preheat = choice(&PREHEATS)?,
            "ends" => self.ends = choice(&ENDS)?,
            "timer" => self.timer_mins = number(1.0, 1440.0, "timer")?.round() as u32,
            "probe_target" => self.probe_target_c = number(1.0, 100.0, "probe target")? as f32,
            "at_end" => self.at_end = choice(&AT_END)?,
            other => return Err(format!("unknown manual cook field `{other}`")),
        }
        Ok(())
    }

    /// Whether the oven goes off at the end: only a stage that ends on its
    /// own can.
    fn turns_off(&self) -> bool {
        self.at_end == TURN_OFF && (self.ends == ENDS_TIMER || self.ends == ENDS_PROBE)
    }

    /// The stages to send, as the web UI's stage editor builds them.
    pub fn stages(&self) -> Vec<Stage> {
        let (top, rear, bottom) = ELEMENTS_ON[self.elements];
        let (timer, probe) = (self.ends == ENDS_TIMER, self.ends == ENDS_PROBE);
        let cook = Stage {
            id: None,
            kind: "cook".into(),
            temperature_c: self.temperature_c,
            temperature_bulbs_mode: Some(if self.mode == 1 { "wet" } else { "dry" }.into()),
            duration_secs: timer.then_some(u64::from(self.timer_mins) * 60),
            timer_added: Some(timer),
            probe_added: Some(probe),
            probe_target_c: probe.then_some(self.probe_target_c),
            steam_pct: self.steam_pct,
            fan_speed: 100,
            // After a preheat that waits: the food goes in, then Next stage.
            user_action_required: Some(self.preheat == PREHEAT_WAIT),
            rack_position: None,
            heating_element_top: Some(top),
            heating_element_rear: Some(rear),
            heating_element_bottom: Some(bottom),
            vent_open: None,
            title: None,
            turn_off_at_end: self.turns_off().then_some(true),
            prompt: None,
        };
        if self.preheat == NO_PREHEAT {
            return vec![cook];
        }
        let preheat = Stage {
            kind: "preheat".into(),
            duration_secs: None,
            timer_added: Some(false),
            probe_added: Some(false),
            probe_target_c: None,
            user_action_required: Some(false),
            turn_off_at_end: None,
            ..cook.clone()
        };
        vec![preheat, cook]
    }

    /// The cook to start.
    pub fn recipe(&self) -> Result<Recipe, String> {
        check(None, self.stages())
    }

    /// The state topic: every field as its entity reads it, a one-line
    /// summary in the oven's display unit, and what's wrong, if anything.
    pub fn state(&self, unit: &str) -> serde_json::Value {
        let problem = self.recipe().err();
        let mut summary: Vec<String> = self
            .stages()
            .iter()
            .map(|s| recipes::summary(s, unit))
            .collect();
        if self.turns_off() {
            summary.push("off".into());
        }
        serde_json::json!({
            "temperature_c": self.temperature_c,
            "mode": MODES[self.mode],
            "steam_pct": self.steam_pct,
            "elements": ELEMENTS[self.elements],
            "preheat": PREHEATS[self.preheat],
            "ends": ENDS[self.ends],
            "timer_mins": self.timer_mins,
            "probe_target_c": self.probe_target_c,
            "at_end": AT_END[self.at_end],
            "summary": summary.join(" → "),
            "problem": problem,
        })
    }
}

/// A manual cook, checked as a saved recipe would be, and titled
/// `Manual cook` unless it has a title of its own.
pub fn check(title: Option<String>, stages: Vec<Stage>) -> Result<Recipe, String> {
    normalize(Recipe {
        id: String::new(),
        title: title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| TITLE.into()),
        stage_count: 0,
        stages,
        source: Default::default(),
    })
    .map_err(|e| match e {
        RecipeError::Invalid(msg) => msg,
        RecipeError::NotFound => "not found".into(),
        RecipeError::Store(e) => format!("{e:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_a_valid_timed_cook() {
        let recipe = Draft::default().recipe().unwrap();
        assert_eq!(recipe.title, "Manual cook");
        assert_eq!(recipe.stages.len(), 1);
        let cook = &recipe.stages[0];
        assert_eq!(cook.duration_secs, Some(1800));
        assert_eq!(cook.heating_element_rear, Some(true));
        assert_eq!(cook.turn_off_at_end, None);
    }

    #[test]
    fn a_preheat_that_waits_leads_a_cook_that_waits() {
        let mut d = Draft::default();
        d.set("preheat", "Preheat, then wait for me").unwrap();
        d.set("at_end", "Turn the oven off").unwrap();
        let stages = d.recipe().unwrap().stages;
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].kind, "preheat");
        assert_eq!(stages[0].user_action_required, Some(false));
        assert_eq!(stages[0].duration_secs, None);
        assert_eq!(stages[1].user_action_required, Some(true));
        assert_eq!(stages[1].turn_off_at_end, Some(true));
    }

    #[test]
    fn fields_from_payloads() {
        let mut d = Draft::default();
        d.set("temperature", "176.66666666666666").unwrap();
        d.set("timer", "45.0").unwrap();
        d.set("elements", "Top + bottom").unwrap();
        d.set("ends", "Probe").unwrap();
        d.set("probe_target", "55").unwrap();
        assert_eq!(d.timer_mins, 45);
        let cook = &d.stages()[0];
        assert_eq!(
            (
                cook.heating_element_top,
                cook.heating_element_rear,
                cook.heating_element_bottom
            ),
            (Some(true), Some(false), Some(true))
        );
        assert_eq!(cook.probe_target_c, Some(55.0));
        assert_eq!(cook.duration_secs, None);

        assert!(d.set("temperature", "300").is_err());
        assert!(d.set("timer", "0").is_err());
        assert!(d.set("mode", "Grill").is_err());
        assert!(d.set("colour", "red").is_err());
        assert_eq!(d.timer_mins, 45);
    }

    #[test]
    fn turning_off_needs_an_end() {
        let mut d = Draft::default();
        d.set("ends", "When I say").unwrap();
        d.set("at_end", "Turn the oven off").unwrap();
        assert_eq!(d.stages()[0].turn_off_at_end, None);
        assert!(!d.state("C")["summary"].as_str().unwrap().ends_with("off"));
    }

    #[test]
    fn state_carries_the_summary_and_any_problem() {
        let mut d = Draft::default();
        d.set("preheat", "Preheat, then start cooking").unwrap();
        let s = d.state("C");
        assert_eq!(
            s["summary"],
            "Preheat · 180 °C dry → Cook · 180 °C dry · 30 min"
        );
        assert_eq!(s["problem"], serde_json::Value::Null);

        d.set("mode", "Sous vide (wet bulb)").unwrap();
        assert_eq!(
            d.state("C")["problem"],
            "stage 1: sous vide temperature must be within 25–100 °C"
        );
    }
}
