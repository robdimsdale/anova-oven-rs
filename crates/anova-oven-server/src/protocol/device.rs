//! Device-side protocol: the frames the oven sends to the cloud endpoint it
//! connects to (`iot-prod.anovaculinary.io`, or a local stand-in).
//!
//! Every frame is `{"action": "<name>", "data": {...}}`. The `state` action
//! carries the shared [`OvenState`] as `data`. See
//! `docs/oven-cloud-emulation.md` for the full protocol.

use serde::Deserialize;

use super::OvenState;

/// Parse a raw text frame from the oven into a protocol event.
pub fn parse_message(data: &[u8]) -> Result<Event, serde_json::Error> {
    let envelope: Envelope = serde_json::from_slice(data)?;
    match envelope.action.as_str() {
        "state" => {
            let msg: StateMessage = serde_json::from_slice(data)?;
            Ok(Event::State(msg.data))
        }
        _ => Ok(Event::Other {
            action: envelope.action,
        }),
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Event {
    /// The oven's complete state, never a diff.
    State(OvenState),
    /// `pairing`, `timezone`, `healthcheck`, `diagnostic` and anything else.
    Other { action: String },
}

#[derive(Deserialize)]
struct Envelope {
    action: String,
}

#[derive(Deserialize)]
struct StateMessage {
    data: OvenState,
}

#[cfg(test)]
mod tests {
    use super::{parse_message, Event};
    use crate::protocol::to_oven_status;

    const COOKING: &str = include_str!("../../testdata/device-state-cooking.json");
    const IDLE: &str = include_str!("../../testdata/device-state-idle.json");

    fn parse_state(raw: &str) -> crate::protocol::OvenState {
        match parse_message(raw.as_bytes()).unwrap() {
            Event::State(state) => state,
            other => panic!("expected state, got {other:?}"),
        }
    }

    #[test]
    fn cooking_state_parses_cook_block() {
        let state = parse_state(COOKING);
        assert_eq!(state.state.processed_command_ids.len(), 10);
        let cook = state.cook.as_ref().expect("cook block");
        assert_eq!(cook.active_stage_index, Some(0));
        assert_eq!(cook.stages.len(), 1);
        assert_eq!(
            cook.cook_id.as_deref(),
            Some("55589dd8-b90e-4d5e-a154-6810258f69ae")
        );

        let oven_cook = crate::protocol::to_oven_cook(&state).expect("oven cook");
        assert_eq!(oven_cook.seconds_elapsed, 1);
        let stage = &oven_cook.stages[0];
        assert_eq!(stage.kind, "cook");
        assert_eq!(stage.title.as_deref(), Some("local probe test"));
        assert_eq!(stage.temperature_c, 60.0);
        assert_eq!(stage.duration_secs, Some(120));
        assert_eq!(stage.heating_element_rear, Some(true));
        assert!(!cook.stage_transition_pending_user_action);
        assert_eq!(cook.seconds_elapsed, Some(1));
        assert_eq!(cook.active_stage_seconds_elapsed, Some(1));

        let status = to_oven_status(&state);
        assert_eq!(status.mode, "cook");
        assert_eq!(status.target_temperature_c, Some(60.0));
        assert_eq!(status.timer_mode, "running");
        assert_eq!(
            status.active_stage_id.as_deref(),
            Some("180718f6-fc16-449f-a94b-95bb0084b524")
        );
    }

    #[test]
    fn idle_state_has_no_cook() {
        let state = parse_state(IDLE);
        assert!(state.cook.is_none());
        assert_eq!(to_oven_status(&state).mode, "idle");
    }

    /// The cooking fixture with its timer mode, pending flag and active
    /// stage changed, and `extra_stages` copies of its one stage appended.
    fn cooking_with(timer_mode: &str, pending: bool, active: usize, extra_stages: usize) -> String {
        let mut v: serde_json::Value = serde_json::from_str(COOKING).unwrap();
        v["data"]["nodes"]["timer"]["mode"] = timer_mode.into();
        let cook = &mut v["data"]["cook"];
        cook["stageTransitionPendingUserAction"] = pending.into();
        cook["activeStageIndex"] = active.into();
        let stage = cook["stages"][0].clone();
        for _ in 0..extra_stages {
            cook["stages"].as_array_mut().unwrap().push(stage.clone());
        }
        v.to_string()
    }

    fn done(raw: &str) -> bool {
        to_oven_status(&parse_state(raw)).cook_done
    }

    #[test]
    fn cook_done_when_last_stage_timer_has_ended() {
        assert!(done(&cooking_with("idle", true, 1, 1)));
    }

    #[test]
    fn cook_not_done_while_running_or_idle() {
        assert!(!done(COOKING));
        assert!(!done(IDLE));
    }

    #[test]
    fn cook_not_done_on_pending_flag_with_timer_running() {
        // START_NEXT_STAGE on the last stage sets the flag with time left.
        assert!(!done(&cooking_with("running", true, 1, 1)));
    }

    #[test]
    fn cook_not_done_while_a_stage_waits_for_the_user() {
        // Stage 0 is done and stage 1 waits for the user.
        assert!(!done(&cooking_with("idle", true, 0, 1)));
    }

    #[test]
    fn cook_not_done_when_stages_were_added() {
        // UPDATE_COOK_STAGES grew the list to 3 stages mid-cook.
        assert!(!done(&cooking_with("idle", true, 1, 2)));
    }

    #[test]
    fn other_actions_are_named() {
        let event = parse_message(br#"{"action": "healthcheck","data": {}}"#).unwrap();
        assert!(matches!(event, Event::Other { action } if action == "healthcheck"));
    }
}
