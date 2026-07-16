//! Server-owned cook stage progression tracking.
//!
//! A single background task owns mutable progression state; its public view
//! ([`anova_oven_api::CookProgress`]) is published through a `watch` channel
//! so `GET /status` can read it without locking. The task drives:
//!
//! - Completion detection for the currently-running stage (timer, probe,
//!   preheat stability).
//! - Flagging `next_stage_ready` when a stage completes so clients can
//!   prompt the user to advance via the phone app.

use std::time::{Duration, Instant};

use anova_oven_api::{CookProgress, CurrentCook, OvenStatus, Stage};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

/// How long the current dry-bulb temperature must sit at or above the preheat
/// target before we call the stage complete.
const PREHEAT_STABILITY_WINDOW: Duration = Duration::from_secs(10);

/// Hysteresis band below the preheat target. Dropping below
/// `target - PREHEAT_HYSTERESIS_C` during the stability window resets the
/// "crossed" timestamp.
const PREHEAT_HYSTERESIS_C: f32 = 2.0;

pub enum CookProgressMsg {
    StartedFromRecipe {
        recipe_title: String,
        stages: Vec<Stage>,
    },
}

/// Internal mutable state owned by the progression task.
struct Tracker {
    /// Stable identity key for the cook (we use `CurrentCook::started_at` —
    /// the Firestore `createdTimestamp`, unique per cook).
    cook_key: String,
    recipe_title: String,
    stages: Vec<Stage>,
    current_stage_index: usize,
    /// When the current stage's dry-bulb temperature first crossed the preheat
    /// target. `None` until we cross (or after hysteresis reset).
    preheat_cross_since: Option<Instant>,
    /// `true` once the current stage's completion criterion has fired **and**
    /// the next stage requires user action.
    next_stage_ready: bool,
    /// Whether we've already logged a "no completion criterion" warning for
    /// the current stage index (log-once per stage).
    warned_no_criterion: bool,
    /// Whether the current stage's timer has ever been observed in "running"
    /// mode. Prevents a false-positive "timer expired" firing when the oven
    /// reports `timer_current_secs == timer_total_secs` before the timer has
    /// actually started.
    timer_has_run: bool,
}

impl Tracker {
    fn current_stage(&self) -> Option<&Stage> {
        self.stages.get(self.current_stage_index)
    }

    fn next_stage(&self) -> Option<&Stage> {
        self.stages.get(self.current_stage_index + 1)
    }

    fn advance_to(&mut self, idx: usize) {
        self.current_stage_index = idx;
        self.preheat_cross_since = None;
        self.next_stage_ready = false;
        self.warned_no_criterion = false;
        self.timer_has_run = false;
    }
}

pub struct CookProgressTask {
    tracker: Option<Tracker>,
    progress_tx: watch::Sender<Option<CookProgress>>,
}

impl CookProgressTask {
    pub fn new(progress_tx: watch::Sender<Option<CookProgress>>) -> Self {
        Self {
            tracker: None,
            progress_tx,
        }
    }

    pub async fn run(
        mut self,
        mut status_rx: watch::Receiver<Option<OvenStatus>>,
        mut current_cook_rx: watch::Receiver<Option<CurrentCook>>,
        mut msg_rx: mpsc::Receiver<CookProgressMsg>,
    ) {
        loop {
            tokio::select! {
                res = status_rx.changed() => {
                    if res.is_err() {
                        warn!("[cook-progress] status channel closed");
                        return;
                    }
                    let status = status_rx.borrow().clone();
                    self.on_status(status.as_ref());
                }
                res = current_cook_rx.changed() => {
                    if res.is_err() {
                        warn!("[cook-progress] current-cook channel closed");
                        return;
                    }
                    let cook = current_cook_rx.borrow().clone();
                    let status_snapshot = status_rx.borrow().clone();
                    self.on_current_cook(cook, status_snapshot.as_ref());
                }
                msg = msg_rx.recv() => {
                    match msg {
                        Some(CookProgressMsg::StartedFromRecipe { recipe_title, stages }) => {
                            self.on_started_from_recipe(recipe_title, stages);
                        }
                        None => {
                            warn!("[cook-progress] command channel closed");
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Seed tracker state immediately from a successful `POST /start` request
    /// so progression logic does not depend on Firestore current-cook
    /// visibility during startup races.
    fn on_started_from_recipe(&mut self, recipe_title: String, stages: Vec<Stage>) {
        if stages.is_empty() {
            return;
        }

        let current_stage_index = 0usize;
        self.tracker = Some(Tracker {
            cook_key: format!("pending-{recipe_title}-{}", stages.len()),
            recipe_title,
            stages,
            current_stage_index,
            preheat_cross_since: None,
            next_stage_ready: false,
            warned_no_criterion: false,
            timer_has_run: false,
        });
        info!(
            stage_count = self.tracker.as_ref().map(|t| t.stages.len()).unwrap_or(0),
            "[cook-progress] tracker seeded from start request"
        );
        self.publish();
    }

    /// Update the tracker in response to a `CurrentCook` change. Preserves
    /// `current_stage_index` when the cook identity is unchanged (e.g. a
    /// periodic Firestore refresh); rebuilds from scratch for a new cook,
    /// inferring the current stage from the latest status.
    fn on_current_cook(&mut self, cook: Option<CurrentCook>, status: Option<&OvenStatus>) {
        let Some(cook) = cook else {
            if self.tracker.is_some() {
                debug!("[cook-progress] cleared tracker (current cook gone)");
            }
            self.tracker = None;
            self.publish();
            return;
        };

        // Live oven state is authoritative for whether a cook is running (the
        // phone app agrees). Anova's Firestore `currentCook` lingers after a
        // cook ends, so a present `cook` does not imply an active cook. If the
        // oven confirms idle, don't (re)build a tracker from a stale record —
        // mirrors `on_status`. A `None` status (mid-startup, not yet observed)
        // still builds, preserving the startup-race behavior below.
        if status.is_some_and(|s| !s.is_cooking()) {
            if self.tracker.is_some() {
                debug!("[cook-progress] ignoring current-cook while oven idle");
                self.tracker = None;
                self.publish();
            }
            return;
        }

        let same_cook = self
            .tracker
            .as_ref()
            .is_some_and(|t| t.cook_key == cook.started_at);

        if same_cook {
            let tracker = self.tracker.as_mut().expect("same_cook implies Some");
            tracker.recipe_title = cook.recipe_title;
            tracker.stages = cook.stages;
            if tracker.current_stage_index >= tracker.stages.len() && !tracker.stages.is_empty() {
                tracker.current_stage_index = tracker.stages.len() - 1;
            }
            debug!(
                stage = tracker.current_stage_index,
                stage_count = tracker.stages.len(),
                "[cook-progress] refreshed stages for ongoing cook"
            );
            self.publish();
            return;
        }

        // Prefer the oven's authoritative `state.cook.activeStageIndex` when
        // present; fall back to the kind/target/timer heuristic only when the
        // oven hasn't yet reported a cook block (e.g. mid-startup).
        let current_stage_index = match status {
            Some(s) if s.is_cooking() => s
                .active_stage_index
                .filter(|i| *i < cook.stages.len())
                .unwrap_or_else(|| infer_stage_index(&cook.stages, s)),
            _ => 0,
        };
        info!(
            cook_key = %cook.started_at,
            recipe = %cook.recipe_title,
            stages = cook.stages.len(),
            current_stage_index,
            source = if status.and_then(|s| s.active_stage_index).is_some() {
                "oven-authoritative"
            } else {
                "heuristic"
            },
            "[cook-progress] tracker rebuilt"
        );

        self.tracker = Some(Tracker {
            cook_key: cook.started_at,
            recipe_title: cook.recipe_title,
            stages: cook.stages,
            current_stage_index,
            preheat_cross_since: None,
            next_stage_ready: false,
            warned_no_criterion: false,
            timer_has_run: false,
        });
        self.publish();
    }

    fn on_status(&mut self, status: Option<&OvenStatus>) {
        let Some(status) = status else { return };

        if !status.is_cooking() {
            if self.tracker.is_some() {
                debug!("[cook-progress] cleared tracker (oven idle)");
                self.tracker = None;
                self.publish();
            }
            return;
        }

        let Some(tracker) = self.tracker.as_mut() else {
            return;
        };

        if tracker.next_stage_ready {
            let next_idx = tracker.current_stage_index + 1;
            if let Some(next_stage) = tracker.stages.get(next_idx) {
                let observed_stage_idx = status
                    .active_stage_index
                    .filter(|i| *i < tracker.stages.len())
                    .unwrap_or_else(|| infer_stage_index(&tracker.stages, status));
                let timer_started =
                    next_stage.duration_secs.unwrap_or(0) > 0 && status.timer_mode == "running";
                let next_stage_observed = observed_stage_idx == next_idx;
                let manual_stage_started = if next_stage.user_action_required == Some(true) {
                    timer_started
                } else {
                    timer_started || next_stage_observed
                };

                if manual_stage_started {
                    info!(
                        next_index = next_idx,
                        observed_stage_index = observed_stage_idx,
                        timer_mode = %status.timer_mode,
                        "[cook-progress] observed next stage start; clearing next-stage-ready"
                    );
                    tracker.advance_to(next_idx);
                    self.publish();
                }
            }
            return;
        }

        let stage_index = tracker.current_stage_index;
        let Some(stage) = tracker.stages.get(stage_index).cloned() else {
            return;
        };

        let complete = evaluate_stage_completion(
            &stage,
            status,
            &mut tracker.preheat_cross_since,
            &mut tracker.warned_no_criterion,
            stage_index,
            &mut tracker.timer_has_run,
        );

        if !complete {
            return;
        }

        info!(
            stage_index = tracker.current_stage_index,
            "[cook-progress] stage complete"
        );

        let next_idx = tracker.current_stage_index + 1;
        if tracker.stages.get(next_idx).is_none() {
            info!("[cook-progress] last stage complete; oven will end cook on its own");
            return;
        }

        // CMD_APO_START_STAGE is rejected by the oven backend with
        // "unauthorized" when sent from our server, so we flag next_stage_ready
        // and let the user advance via the phone app.
        tracker.next_stage_ready = true;
        self.publish();
    }

    fn publish(&self) {
        let progress = self.tracker.as_ref().map(build_cook_progress);
        let _ = self.progress_tx.send(progress);
    }
}

fn build_cook_progress(tracker: &Tracker) -> CookProgress {
    let current = tracker.current_stage();
    let current_description = current
        .map(Stage::short_description)
        .unwrap_or_else(|| "Unknown stage".into());
    let current_kind = current
        .map(|s| s.kind.clone())
        .unwrap_or_else(|| "cook".into());
    let next_description = if tracker.next_stage_ready {
        tracker.next_stage().map(Stage::short_description)
    } else {
        None
    };

    CookProgress {
        recipe_title: tracker.recipe_title.clone(),
        current_stage_index: tracker.current_stage_index,
        total_stage_count: tracker.stages.len(),
        current_stage_description: current_description,
        current_stage_kind: current_kind,
        next_stage_ready: tracker.next_stage_ready,
        next_stage_description: next_description,
    }
}

/// Evaluate the three completion heuristics (timer, probe, preheat + hold)
/// for `stage` given the latest `status`. Mutates `preheat_cross_since` to
/// track the preheat stability window.
fn evaluate_stage_completion(
    stage: &Stage,
    status: &OvenStatus,
    preheat_cross_since: &mut Option<Instant>,
    warned_no_criterion: &mut bool,
    stage_index: usize,
    timer_has_run: &mut bool,
) -> bool {
    // Track whether the timer has been observed running. This guards against
    // falsely declaring a timed stage complete when the oven initialises
    // `timer_current_secs` to the stage's full duration before the user has
    // actually started the timer (manual-start stages).
    if status.timer_mode == "running" {
        *timer_has_run = true;
    }

    // Timer expired. Some stage payloads include a timer object even when the
    // timer is not actually enabled; prefer `timer_added` when present.
    let has_timer = if let Some(timer_added) = stage.timer_added {
        timer_added && stage.duration_secs.unwrap_or(0) > 0
    } else {
        stage.duration_secs.unwrap_or(0) > 0
    };
    if has_timer
        && *timer_has_run
        && status.timer_total_secs > 0
        && status.timer_current_secs >= status.timer_total_secs
        && status.timer_mode != "running"
    {
        return true;
    }

    // Probe target reached. Firestore/current-cook payloads can contain a
    // default probe setpoint even when probe mode is off; require
    // `probe_added == true` when that flag is available.
    let has_probe = if let Some(probe_added) = stage.probe_added {
        probe_added && stage.probe_target_c.is_some()
    } else {
        stage.probe_target_c.is_some()
    };
    if has_probe {
        if let (Some(target), Some(current)) = (stage.probe_target_c, status.probe_temperature_c) {
            if current >= target {
                return true;
            }
        }
    }

    // Preheat stability (only when neither timer nor probe applies).
    let is_preheat = stage.kind == "preheat";
    if !has_timer && !has_probe {
        if is_preheat {
            let target = stage.temperature_c;
            let current = status.current_temperature_c();
            let hysteresis_floor = target - PREHEAT_HYSTERESIS_C;
            match *preheat_cross_since {
                None if current >= target => {
                    *preheat_cross_since = Some(Instant::now());
                }
                Some(_) if current < hysteresis_floor => {
                    *preheat_cross_since = None;
                }
                Some(since) if since.elapsed() >= PREHEAT_STABILITY_WINDOW => {
                    return true;
                }
                _ => {}
            }
        } else if !*warned_no_criterion {
            warn!(
                stage_index,
                "[cook-progress] stage has no timer, probe, or preheat criterion; will never complete"
            );
            *warned_no_criterion = true;
        }
    }

    false
}

/// Heuristic to pick the current stage index from `status` when rebuilding the
/// tracker mid-cook (e.g. after a server restart). Matches on kind, target
/// temperature (±1 °C), and timer total when set. Falls back to the first
/// stage whose kind matches `status.stage_kind()`.
fn infer_stage_index(stages: &[Stage], status: &OvenStatus) -> usize {
    let status_kind = status.stage_kind();
    let target = status.target_temperature_c;

    let mut precise_matches = stages.iter().enumerate().filter(|(_, s)| {
        if s.kind != status_kind {
            return false;
        }
        if let Some(t) = target {
            if (s.temperature_c - t).abs() > 1.0 {
                return false;
            }
        }
        if status.timer_total_secs > 0 {
            if let Some(dur) = s.duration_secs {
                if dur != status.timer_total_secs {
                    return false;
                }
            }
        }
        true
    });

    if let Some((idx, _)) = precise_matches.next() {
        if precise_matches.next().is_none() {
            return idx;
        }
        warn!("[cook-progress] ambiguous stage match on rebuild; falling back to first kind match");
    } else {
        warn!(
            "[cook-progress] no precise stage match on rebuild; falling back to first kind match"
        );
    }

    stages
        .iter()
        .position(|s| s.kind == status_kind)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{evaluate_stage_completion, CookProgressTask};
    use anova_oven_api::{CookProgress, CurrentCook, OvenStatus, Stage};
    use tokio::sync::watch;

    fn task_with_rx() -> (CookProgressTask, watch::Receiver<Option<CookProgress>>) {
        let (tx, rx) = watch::channel(None);
        (CookProgressTask::new(tx), rx)
    }

    fn idle_status() -> OvenStatus {
        OvenStatus {
            mode: "idle".into(),
            ..base_status()
        }
    }

    fn stale_cook() -> CurrentCook {
        // Anova's Firestore currentCook still holds this record after the cook
        // ended and the oven went idle — the scenario that drove the phantom
        // "tracker rebuilt" log spam.
        CurrentCook {
            recipe_id: None,
            recipe_title: "Steam Oven Toast".into(),
            started_at: "2026-07-15T13:28:27.750Z".into(),
            cook_stage_count: 3,
            total_stage_count: 3,
            stages: vec![base_stage(), base_stage(), base_stage()],
        }
    }

    fn base_stage() -> Stage {
        Stage {
            id: Some("stage-1".into()),
            kind: "cook".into(),
            temperature_c: 25.0,
            temperature_bulbs_mode: Some("wet".into()),
            duration_secs: None,
            timer_added: Some(false),
            probe_added: Some(false),
            probe_target_c: None,
            steam_pct: 0.0,
            fan_speed: 75,
            user_action_required: Some(false),
            rack_position: Some(3),
            heating_element_top: Some(false),
            heating_element_rear: Some(false),
            heating_element_bottom: Some(true),
            vent_open: Some(false),
            title: Some("Stage".into()),
        }
    }

    fn base_status() -> OvenStatus {
        OvenStatus {
            mode: "cook".into(),
            temperature_unit: "F".into(),
            temperature_c: 25.0,
            target_temperature_c: Some(25.0),
            temperature_bulbs_mode: "wet".into(),
            dry_top_temperature_c: 25.0,
            dry_bottom_temperature_c: 25.0,
            wet_bulb_temperature_c: 25.0,
            probe_temperature_c: Some(0.0),
            timer_current_secs: 0,
            timer_total_secs: 0,
            timer_mode: "idle".into(),
            steam_pct: 0.0,
            steam_target_pct: Some(0.0),
            steam_generator_mode: "idle".into(),
            boiler_celsius: 0.0,
            boiler_watts: 0.0,
            boiler_descale_required: false,
            evaporator_celsius: 0.0,
            evaporator_watts: 0.0,
            fan_speed: 75,
            heating_element_top_on: false,
            heating_element_top_watts: 0.0,
            heating_element_rear_on: false,
            heating_element_rear_watts: 0.0,
            heating_element_bottom_on: true,
            heating_element_bottom_watts: 0.0,
            lamp_on: false,
            lamp_preference: "off".into(),
            vent_open: false,
            door_open: false,
            water_tank_empty: false,
            active_stage_index: None,
            active_stage_id: None,
            cook_progress: None,
            upstream: None,
        }
    }

    #[test]
    fn probe_target_is_ignored_when_probe_not_added() {
        let mut stage = base_stage();
        // Some Anova payloads include a default probe setpoint even when the
        // stage is not probe-driven.
        stage.probe_added = Some(false);
        stage.probe_target_c = Some(0.0);

        let status = base_status();
        let mut preheat_cross_since = None;
        let mut warned_no_criterion = false;
        let mut timer_has_run = false;

        let complete = evaluate_stage_completion(
            &stage,
            &status,
            &mut preheat_cross_since,
            &mut warned_no_criterion,
            1,
            &mut timer_has_run,
        );

        assert!(!complete);
    }

    #[test]
    fn manual_timer_stage_is_not_complete_before_timer_runs() {
        let mut stage = base_stage();
        stage.duration_secs = Some(3600);
        stage.timer_added = Some(true);
        stage.user_action_required = Some(true);

        let mut status = base_status();
        status.timer_mode = "idle".into();
        status.timer_total_secs = 3600;
        status.timer_current_secs = 3600;

        let mut preheat_cross_since = None;
        let mut warned_no_criterion = false;
        let mut timer_has_run = false;

        let complete = evaluate_stage_completion(
            &stage,
            &status,
            &mut preheat_cross_since,
            &mut warned_no_criterion,
            1,
            &mut timer_has_run,
        );

        assert!(!complete);
    }

    // A stale Firestore currentCook must not build a tracker while the oven is
    // confirmed idle — this is the phantom "tracker rebuilt" loop.
    #[test]
    fn stale_cook_builds_no_tracker_when_oven_idle() {
        let (mut task, rx) = task_with_rx();

        task.on_current_cook(Some(stale_cook()), Some(&idle_status()));

        assert!(task.tracker.is_none());
        assert!(rx.borrow().is_none());
    }

    // A live idle status clears a tracker that was built while cooking, and a
    // subsequent stale-cook refresh does not resurrect it.
    #[test]
    fn idle_status_clears_tracker_and_stale_refresh_does_not_rebuild() {
        let (mut task, rx) = task_with_rx();

        // Cooking: tracker is built.
        task.on_current_cook(Some(stale_cook()), Some(&base_status()));
        assert!(task.tracker.is_some());
        assert!(rx.borrow().is_some());

        // Oven goes idle: the same-cook current-cook update must clear it.
        task.on_current_cook(Some(stale_cook()), Some(&idle_status()));
        assert!(task.tracker.is_none());
        assert!(rx.borrow().is_none());
    }

    // Status unknown (mid-startup, oven not yet observed) still builds, so a
    // genuine cook in progress at startup is tracked.
    #[test]
    fn current_cook_builds_tracker_when_status_unknown() {
        let (mut task, _rx) = task_with_rx();

        task.on_current_cook(Some(stale_cook()), None);

        assert!(task.tracker.is_some());
    }
}
