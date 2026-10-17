//! Milestone events for Home Assistant: edges, not states.
//!
//! [`milestones`] is a pure function of two [`Snapshot`]s, the previous one
//! and the new one. The first snapshot after a start is the baseline and
//! fires nothing, so a server restart mid-cook doesn't announce
//! `cook_started` again. One edge, one event: a preheat whose next stage
//! waits for the user ends with `stage_waiting`, and `preheat_done` follows
//! only when the user starts that stage.
//!
//! The stage comes from the server's cook progress, which in local mode
//! follows the oven's own `activeStageIndex` and
//! `stageTransitionPendingUserAction`. In cloud mode it's inferred, so
//! `stage_waiting` can be late or missed, and since cloud mode never sets
//! `cook_done`, every end of a cook there is `cook_stopped`.

use anova_oven_api::{CurrentCook, OvenStatus, Stage};

use super::recipes;

/// What the milestones are worked out from.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// The oven's connection to the server.
    pub connected: bool,
    pub cooking: bool,
    /// The oven's `cookId`, which identifies the cook. Not the current
    /// cook's start time: in local mode that moves from the state machine's
    /// placeholder to the journal's once, mid-cook.
    pub cook_key: Option<String>,
    pub recipe_title: Option<String>,
    pub stage_index: Option<usize>,
    pub stages: Vec<Stage>,
    pub next_stage_ready: bool,
    pub cook_done: bool,
    /// The oven's display unit, for the stage summaries.
    pub unit: String,
}

impl Snapshot {
    /// From the full `/status` (with `cook_progress` attached) and the
    /// current cook.
    pub fn new(status: Option<&OvenStatus>, cook: Option<&CurrentCook>, connected: bool) -> Self {
        let progress = status.and_then(|s| s.cook_progress.as_ref());
        Snapshot {
            connected,
            cooking: status.is_some_and(OvenStatus::is_cooking),
            cook_key: status.and_then(|s| s.cook_id.clone()),
            recipe_title: cook.map(|c| c.recipe_title.clone()),
            stage_index: progress
                .map(|p| p.current_stage_index)
                .or_else(|| status.and_then(|s| s.active_stage_index)),
            stages: cook.map(|c| c.stages.clone()).unwrap_or_default(),
            next_stage_ready: progress.is_some_and(|p| p.next_stage_ready),
            cook_done: status.is_some_and(|s| s.cook_done),
            unit: status.map_or_else(|| "C".into(), |s| s.temperature_unit.clone()),
        }
    }

    fn stage(&self, index: Option<usize>) -> Option<&Stage> {
        self.stages.get(index?)
    }

    /// The event's payload: what a notification needs to say which cook and
    /// stage it's about.
    pub fn payload(&self, kind: &str) -> serde_json::Value {
        serde_json::json!({
            "event_type": kind,
            "cook_id": self.cook_key,
            "recipe_title": self.recipe_title,
            "stage_index": self.stage_index,
            "stage_count": self.stages.len(),
            "current_stage": self.stage(self.stage_index).map(|s| recipes::summary(s, &self.unit)),
            "next_stage": self
                .stage(self.stage_index.map(|i| i + 1))
                .map(|s| recipes::summary(s, &self.unit)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Milestone {
    CookStarted,
    PreheatDone,
    StageStarted,
    StageWaiting,
    CookDone,
    CookStopped,
    OvenLost,
}

impl Milestone {
    pub fn as_str(self) -> &'static str {
        match self {
            Milestone::CookStarted => "cook_started",
            Milestone::PreheatDone => "preheat_done",
            Milestone::StageStarted => "stage_started",
            Milestone::StageWaiting => "stage_waiting",
            Milestone::CookDone => "cook_done",
            Milestone::CookStopped => "cook_stopped",
            Milestone::OvenLost => "oven_lost",
        }
    }

    /// Whether the event is about the cook as it was (it has just gone).
    pub fn describes_previous(self) -> bool {
        matches!(self, Milestone::CookStopped | Milestone::OvenLost)
    }
}

/// Every `event_type` the event entity announces.
pub const EVENT_TYPES: [&str; 8] = [
    "cook_started",
    "preheat_done",
    "stage_started",
    "stage_waiting",
    "cook_done",
    "cook_stopped",
    "command_failed",
    "oven_lost",
];

/// The milestones between two snapshots.
pub fn milestones(prev: &Snapshot, next: &Snapshot) -> Vec<Milestone> {
    let mut out = Vec::new();

    if prev.connected && !next.connected && prev.cooking {
        out.push(Milestone::OvenLost);
    }
    // While the oven is away its last state stands; nothing else to say.
    if !next.connected {
        return out;
    }

    match (prev.cooking, next.cooking) {
        (false, true) => out.push(Milestone::CookStarted),
        (true, false) => {
            // A cook that reached its end already said `cook_done`.
            if !prev.cook_done {
                out.push(Milestone::CookStopped);
            }
        }
        (true, true) => {
            let new_cook = matches!(
                (&prev.cook_key, &next.cook_key),
                (Some(a), Some(b)) if a != b
            );
            if new_cook {
                out.push(Milestone::CookStarted);
                return out;
            }
            if let (Some(before), Some(after)) = (prev.stage_index, next.stage_index) {
                if before != after {
                    let was_preheat = prev
                        .stage(Some(before))
                        .is_some_and(|s| s.kind == "preheat");
                    out.push(if was_preheat {
                        Milestone::PreheatDone
                    } else {
                        Milestone::StageStarted
                    });
                }
            }
            if !prev.next_stage_ready && next.next_stage_ready {
                out.push(Milestone::StageWaiting);
            }
            if !prev.cook_done && next.cook_done {
                out.push(Milestone::CookDone);
            }
        }
        (false, false) => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processors::mqtt::recipes::tests::stage;

    fn cooking(stage_index: usize) -> Snapshot {
        Snapshot {
            connected: true,
            cooking: true,
            cook_key: Some("cook-1".into()),
            recipe_title: Some("Bread".into()),
            stage_index: Some(stage_index),
            stages: vec![stage("preheat"), stage("cook"), stage("cook")],
            unit: "C".into(),
            ..Default::default()
        }
    }

    fn idle() -> Snapshot {
        Snapshot {
            connected: true,
            ..Default::default()
        }
    }

    fn sequence(snapshots: &[Snapshot]) -> Vec<&'static str> {
        snapshots
            .windows(2)
            .flat_map(|w| milestones(&w[0], &w[1]))
            .map(Milestone::as_str)
            .collect()
    }

    #[test]
    fn a_multi_stage_cook_produces_each_milestone_once() {
        let waiting = Snapshot {
            next_stage_ready: true,
            ..cooking(1)
        };
        let done = Snapshot {
            cook_done: true,
            ..cooking(2)
        };
        assert_eq!(
            sequence(&[
                idle(),
                cooking(0),
                cooking(0), // a repeat report: nothing
                cooking(1), // the preheat ended
                waiting.clone(),
                waiting,    // still waiting: nothing
                cooking(2), // the user started stage 3
                done.clone(),
                done,
                idle(), // stopped after the end: no cook_stopped
            ]),
            [
                "cook_started",
                "preheat_done",
                "stage_waiting",
                "stage_started",
                "cook_done"
            ]
        );
    }

    #[test]
    fn a_preheat_waiting_for_the_user_says_waiting_then_done() {
        let waiting = Snapshot {
            next_stage_ready: true,
            ..cooking(0)
        };
        assert_eq!(
            sequence(&[cooking(0), waiting, cooking(1)]),
            ["stage_waiting", "preheat_done"]
        );
    }

    #[test]
    fn stopping_early_is_cook_stopped() {
        assert_eq!(sequence(&[cooking(1), idle()]), ["cook_stopped"]);
    }

    #[test]
    fn a_new_cook_over_a_running_one_is_cook_started() {
        let replaced = Snapshot {
            cook_key: Some("cook-2".into()),
            ..cooking(0)
        };
        assert_eq!(sequence(&[cooking(1), replaced]), ["cook_started"]);
        // A cook whose record arrives late is the same cook.
        let unknown = Snapshot {
            cook_key: None,
            ..cooking(0)
        };
        assert!(sequence(&[unknown, cooking(0)]).is_empty());
    }

    #[test]
    fn losing_the_oven_mid_cook_is_oven_lost_and_nothing_else() {
        let lost = Snapshot {
            connected: false,
            ..cooking(1)
        };
        assert_eq!(
            sequence(&[cooking(1), lost.clone(), cooking(1)]),
            ["oven_lost"]
        );
        let idle_lost = Snapshot {
            connected: false,
            ..idle()
        };
        assert!(sequence(&[idle(), idle_lost]).is_empty());
    }

    #[test]
    fn payload_names_the_cook_and_stages() {
        let p = cooking(0).payload("cook_started");
        assert_eq!(p["event_type"], "cook_started");
        assert_eq!(p["cook_id"], "cook-1");
        assert_eq!(p["stage_count"], 3);
        assert_eq!(p["current_stage"], "Preheat · 220 °C dry");
        assert_eq!(p["next_stage"], "Cook · 220 °C dry");
    }
}
