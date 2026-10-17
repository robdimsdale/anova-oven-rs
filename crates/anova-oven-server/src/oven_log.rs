//! Logging of what the oven itself does, beyond what a recipe asks for.
//!
//! Some outputs aren't part of a stage at all (the lamp, the lamp
//! preference) or are set by the firmware on its own (the fan and vent at the
//! end of a cook), so the only way to explain a change is to see it next to
//! the stage that was running. [`OvenLog`] logs, from successive oven
//! reports:
//!
//! - `[oven] outputs`: the lamp, lamp preference, fan and vent on the first
//!   report, then whenever one of them changes, with the stage at the time;
//! - `[oven] stage started`: the settings of each stage as it becomes
//!   active, as the oven holds them.
//!
//! Heating elements are left out of the change log: the oven switches them
//! on and off all the time to hold a temperature. The stage line has the
//! ones the stage selects.

use std::fmt;

use anova_oven_api::{OvenStatus, Stage};
use tracing::info;

/// The outputs [`OvenLog`] watches.
#[derive(Clone, Debug, PartialEq)]
struct Outputs {
    lamp_on: bool,
    lamp_preference: String,
    fan_speed: u32,
    vent_open: bool,
}

impl Outputs {
    fn of(status: &OvenStatus) -> Self {
        Self {
            lamp_on: status.lamp_on,
            lamp_preference: status.lamp_preference.clone(),
            fan_speed: status.fan_speed,
            vent_open: status.vent_open,
        }
    }
}

impl fmt::Display for Outputs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lamp={} lamp_preference={} fan={} vent={}",
            on_off(self.lamp_on),
            self.lamp_preference,
            self.fan_speed,
            if self.vent_open { "open" } else { "closed" },
        )
    }
}

/// Remembers the previous report, to log only what changed.
#[derive(Debug, Default)]
pub struct OvenLog {
    outputs: Option<Outputs>,
    /// The active stage of the cook in the previous report, by index and
    /// cook, so a new cook's first stage logs even at the same index.
    stage: Option<(Option<String>, usize)>,
}

impl OvenLog {
    /// Log what changed since the previous report. `stages` is the oven's own
    /// stage list for the running cook (empty when idle). Returns the lines
    /// logged, for tests.
    pub fn observe(&mut self, status: &OvenStatus, stages: &[Stage]) -> Vec<Logged> {
        let mut logged = Vec::new();
        let stage = status
            .active_stage_index
            .map(|i| (status.cook_id.clone(), i));
        if let Some((_, index)) = stage.as_ref().filter(|_| stage != self.stage) {
            match stages.get(*index) {
                Some(s) => info!(
                    index,
                    of = stages.len(),
                    title = s.title.as_deref().unwrap_or(""),
                    kind = %s.kind,
                    mode = s.temperature_bulbs_mode.as_deref().unwrap_or("dry"),
                    temp_c = format!("{:.1}", s.temperature_c),
                    elements = %elements(s),
                    fan = s.fan_speed,
                    vent = on_off(s.vent_open.unwrap_or(false)),
                    steam_pct = s.steam_pct,
                    timer_secs = ?s.duration_secs,
                    probe_c = ?s.probe_target_c,
                    "[oven] stage started"
                ),
                None => info!(index, of = stages.len(), "[oven] stage started"),
            }
            logged.push(Logged::StageStarted(*index));
        }
        self.stage = stage;

        let outputs = Outputs::of(status);
        match &self.outputs {
            None => {
                info!(outputs = %outputs, stage = ?status.active_stage_index, "[oven] outputs");
                logged.push(Logged::Outputs(outputs.to_string()));
            }
            Some(previous) if *previous != outputs => {
                info!(
                    outputs = %outputs,
                    was = %previous,
                    stage = ?status.active_stage_index,
                    mode = %status.mode,
                    "[oven] outputs changed"
                );
                logged.push(Logged::Outputs(outputs.to_string()));
            }
            Some(_) => {}
        }
        self.outputs = Some(outputs);
        logged
    }
}

/// A line [`OvenLog::observe`] logged.
#[derive(Debug, PartialEq)]
pub enum Logged {
    StageStarted(usize),
    Outputs(String),
}

fn on_off(on: bool) -> &'static str {
    if on {
        "on"
    } else {
        "off"
    }
}

/// The elements a stage selects, e.g. `top+rear`.
fn elements(stage: &Stage) -> String {
    let on: Vec<&str> = [
        ("top", stage.heating_element_top),
        ("bottom", stage.heating_element_bottom),
        ("rear", stage.heating_element_rear),
    ]
    .into_iter()
    .filter(|(_, on)| *on == Some(true))
    .map(|(name, _)| name)
    .collect();
    if on.is_empty() {
        "none".into()
    } else {
        on.join("+")
    }
}

#[cfg(test)]
mod tests {
    use super::{Logged, OvenLog};
    use crate::protocol::{self, device};

    /// The recorded cooking state, on stage `index` with the lamp `lamp`.
    fn cooking(
        index: usize,
        lamp: bool,
    ) -> (anova_oven_api::OvenStatus, Vec<anova_oven_api::Stage>) {
        let mut v: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/device-state-cooking.json")).unwrap();
        // A second stage like Toast's cool-down: 25 °C on the bottom element.
        let mut cool = v["data"]["cook"]["stages"][0].clone();
        cool["title"] = "Cool down".into();
        cool["temperatureBulbs"]["dry"]["setpoint"]["celsius"] = 25.into();
        cool["heatingElements"] = serde_json::json!({
            "top": {"on": false}, "bottom": {"on": true}, "rear": {"on": false}
        });
        v["data"]["cook"]["stages"]
            .as_array_mut()
            .unwrap()
            .push(cool);
        v["data"]["cook"]["activeStageIndex"] = index.into();
        v["data"]["nodes"]["lamp"]["on"] = lamp.into();
        let device::Event::State(state) = device::parse_message(v.to_string().as_bytes()).unwrap()
        else {
            panic!("not a state");
        };
        let stages = protocol::to_oven_cook(&state)
            .map(|c| c.stages)
            .unwrap_or_default();
        (protocol::to_oven_status(&state), stages)
    }

    #[test]
    fn logs_outputs_once_then_only_changes_and_new_stages() {
        let mut log = OvenLog::default();
        let (status, stages) = cooking(0, true);
        assert_eq!(stages.len(), 2);
        assert_eq!(super::elements(&stages[1]), "bottom");
        assert_eq!(stages[1].title.as_deref(), Some("Cool down"));

        let first = log.observe(&status, &stages);
        assert_eq!(first[0], Logged::StageStarted(0));
        assert!(matches!(&first[1], Logged::Outputs(o) if o.contains("lamp=on")));

        // The same report again logs nothing.
        assert!(log.observe(&status, &stages).is_empty());

        // The next stage starts and the lamp goes off with it.
        let (status, stages) = cooking(1, false);
        let next = log.observe(&status, &stages);
        assert_eq!(next[0], Logged::StageStarted(1));
        assert!(
            matches!(&next[1], Logged::Outputs(o) if o.contains("lamp=off")),
            "{next:?}"
        );
    }
}
