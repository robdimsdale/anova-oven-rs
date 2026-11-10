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
//!
//! In local mode the oven's own state is complete, so none of that is
//! inferred: the stage is `activeStageIndex`, and `next_stage_ready` is
//! `stageTransitionPendingUserAction` with a stage still to come, or a stage
//! that holds until the user moves on (see [`follow_oven`]).
//!
//! The task also times the current stage ([`StageTimes`]): when it started,
//! and when its timer ends.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anova_oven_api::{CookProgress, CurrentCook, OvenStatus, Stage};
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::liveness::Liveness;

/// How long the current dry-bulb temperature must sit at or above the preheat
/// target before we call the stage complete.
const PREHEAT_STABILITY_WINDOW: Duration = Duration::from_secs(10);

/// Hysteresis band below the preheat target. Dropping below
/// `target - PREHEAT_HYSTERESIS_C` during the stability window resets the
/// "crossed" timestamp.
const PREHEAT_HYSTERESIS_C: f32 = 2.0;

/// A gap between two oven reports longer than this means the server wasn't
/// watching: the oven, or the server's link to it, was away. The oven
/// reports every 5 s while it cooks and every 30 s while idle.
const MAX_REPORT_GAP_MS: u64 = 60_000;

/// A new end time this close to the last one keeps the last one. The oven
/// reports whole seconds, so the same end works out a second either way,
/// and a countdown would jump. The kept end is the one compared, so a slow
/// drift builds up and then moves the end by a few seconds at once.
const END_WOBBLE_SECS: i64 = 2;

/// The cook and the oven stage a report is about, where known. The oven can
/// leave either out for a moment: a cook ID that arrives late, or a stage
/// index not reported yet.
///
/// The one rule for "another cook" and "another stage", used by the stage
/// clock here and by the milestones (`stage_started` and friends), so the
/// two agree on what counts as a new stage or cook. They can still differ
/// on when: the milestones compare MQTT polls, so they may miss a stage
/// shorter than a poll, hold back `stage_started` while the next stage
/// waits for the user, and announce a stage that began while the oven was
/// away, whose `stage_started_at` is `null`.
/// A part that is unknown on either side is no change: the server can't
/// tell, so it doesn't guess.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StageKey {
    pub cook_id: Option<String>,
    pub stage_index: Option<usize>,
}

impl StageKey {
    /// Another cook: both cook IDs known, and different.
    pub fn new_cook(&self, next: &StageKey) -> bool {
        matches!((&self.cook_id, &next.cook_id), (Some(a), Some(b)) if a != b)
    }

    /// Another stage: both stage indices known, and different.
    pub fn new_stage(&self, next: &StageKey) -> bool {
        matches!((self.stage_index, next.stage_index), (Some(a), Some(b)) if a != b)
    }
}

/// When an oven report arrived, and whether the server saw the oven without
/// a break up to it.
#[derive(Clone, Copy, Debug)]
struct Arrival {
    /// Unix seconds.
    at: i64,
    /// The report before it came at most [`MAX_REPORT_GAP_MS`] earlier.
    watched: bool,
}

/// The arrival of the last report (`last`, Unix ms), from it and the one
/// before it. Watched only when the gap between them was short.
fn arrival_from(last: Option<u64>, previous: Option<u64>, now: i64) -> Arrival {
    match last {
        Some(last) => Arrival {
            at: (last / 1000) as i64,
            watched: previous.is_some_and(|p| last.saturating_sub(p) <= MAX_REPORT_GAP_MS),
        },
        None => Arrival {
            at: now,
            watched: false,
        },
    }
}

/// The current stage's start and end, as [`CookProgress::stage_started_at`]
/// and [`CookProgress::stage_ends_at`] carry them.
///
/// The start is the arrival of the first report that shows the stage, so it
/// is up to one report (5 s) late. A start the server didn't see is `None`,
/// never a guess: one that came before the server's first report, or after
/// a gap of more than [`MAX_REPORT_GAP_MS`] (the oven was away). A gap that
/// short is bridged: a stage that began in it is timed from the first
/// report after it.
#[derive(Debug, Default)]
struct StageTimes {
    /// The stage the times are for; `None` while the oven doesn't cook.
    key: Option<StageKey>,
    started_at: Option<i64>,
    end: StageEnd,
}

impl StageTimes {
    /// Follow one report. `key` is `None` while the oven doesn't cook.
    fn update(&mut self, key: Option<StageKey>, remaining: Option<u64>, total: u64, at: Arrival) {
        let starts = match (&self.key, &key) {
            (_, None) => {
                self.started_at = None;
                false
            }
            (None, Some(_)) => true,
            (Some(before), Some(after)) => before.new_cook(after) || before.new_stage(after),
        };
        if starts {
            self.started_at = at.watched.then_some(at.at);
        }
        self.key = key;
        self.end.update(remaining, total, at.at);
    }
}

/// The end time of the running timer. The oven reports the time left only
/// now and then, so the end is worked out from when each report arrived,
/// and stays put between reports.
#[derive(Debug, Default)]
struct StageEnd {
    /// The timer's total and its end, in Unix seconds.
    anchor: Option<(u64, i64)>,
}

impl StageEnd {
    fn update(&mut self, remaining: Option<u64>, total: u64, at: i64) {
        let Some(remaining) = remaining else {
            self.anchor = None;
            return;
        };
        let ends_at = at + remaining as i64;
        // A timer set again (a new total) always moves the end.
        let keep = self
            .anchor
            .is_some_and(|(t, end)| t == total && (end - ends_at).abs() <= END_WOBBLE_SECS);
        if !keep {
            self.anchor = Some((total, ends_at));
        }
    }

    fn ends_at(&self) -> Option<i64> {
        self.anchor.map(|(_, end)| end)
    }
}

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
    /// Local mode: take the stage and the waiting-for-you flag from the oven
    /// instead of inferring them.
    follow_oven: bool,
    /// How far short of its target a preheat counts as reached.
    preheat_tolerance_c: f32,
    times: StageTimes,
    /// When the oven's reports arrived. `None` in tests.
    liveness: Option<Arc<Liveness>>,
}

impl CookProgressTask {
    pub fn new(
        progress_tx: watch::Sender<Option<CookProgress>>,
        follow_oven: bool,
        preheat_tolerance_c: f32,
    ) -> Self {
        Self {
            tracker: None,
            progress_tx,
            follow_oven,
            preheat_tolerance_c,
            times: StageTimes::default(),
            liveness: None,
        }
    }

    /// Time the stages from when the oven's reports arrived.
    pub fn with_liveness(mut self, liveness: Arc<Liveness>) -> Self {
        self.liveness = Some(liveness);
        self
    }

    /// The last report's arrival. Without [`Liveness`] (tests), now, seen.
    ///
    /// Only fresh frames are recorded. In cloud mode a stale frame (the oven
    /// not reaching Anova's cloud) still updates the status, so a new stage
    /// in one is timed from the fresh frame before it.
    fn arrival(&self) -> Arrival {
        let now = jiff::Timestamp::now().as_second();
        match &self.liveness {
            Some(liveness) => {
                let (last, previous) = liveness.last_two_states_ms();
                arrival_from(last, previous, now)
            }
            None => Arrival {
                at: now,
                watched: true,
            },
        }
    }

    /// Time the stage the oven is in now. The stage index is the one
    /// [`CookProgress`] carries, else the oven's own, as the milestones read
    /// it.
    fn update_times(&mut self, status: Option<&OvenStatus>, at: Arrival) {
        // Mid-startup: nothing is known yet.
        let Some(status) = status else { return };
        let key = status.is_cooking().then(|| StageKey {
            cook_id: status.cook_id.clone(),
            stage_index: self
                .tracker
                .as_ref()
                .map(|t| t.current_stage_index)
                .or(status.active_stage_index),
        });
        self.times.update(
            key,
            status.timer_remaining_secs(),
            status.timer_total_secs,
            at,
        );
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
        self.follow_cook(cook, status);
        let at = self.arrival();
        self.update_times(status, at);
        self.publish();
    }

    fn follow_cook(&mut self, cook: Option<CurrentCook>, status: Option<&OvenStatus>) {
        let Some(cook) = cook else {
            if self.tracker.is_some() {
                debug!("[cook-progress] cleared tracker (current cook gone)");
            }
            self.tracker = None;
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
            if self.follow_oven {
                if let Some(status) = status {
                    follow_oven(tracker, status);
                }
            }
            debug!(
                stage = tracker.current_stage_index,
                stage_count = tracker.stages.len(),
                "[cook-progress] refreshed stages for ongoing cook"
            );
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

        let mut tracker = Tracker {
            cook_key: cook.started_at,
            recipe_title: cook.recipe_title,
            stages: cook.stages,
            current_stage_index,
            preheat_cross_since: None,
            next_stage_ready: false,
            warned_no_criterion: false,
            timer_has_run: false,
        };
        if self.follow_oven {
            if let Some(status) = status {
                follow_oven(&mut tracker, status);
            }
        }
        self.tracker = Some(tracker);
    }

    fn on_status(&mut self, status: Option<&OvenStatus>) {
        let at = self.arrival();
        self.on_status_at(status, at);
    }

    fn on_status_at(&mut self, status: Option<&OvenStatus>, at: Arrival) {
        self.follow_status(status);
        self.update_times(status, at);
        self.publish();
    }

    fn follow_status(&mut self, status: Option<&OvenStatus>) {
        let Some(status) = status else { return };

        if !status.is_cooking() {
            if self.tracker.is_some() {
                debug!("[cook-progress] cleared tracker (oven idle)");
                self.tracker = None;
            }
            return;
        }

        let Some(tracker) = self.tracker.as_mut() else {
            return;
        };

        if self.follow_oven {
            if follow_oven(tracker, status) {
                info!(
                    stage_index = tracker.current_stage_index,
                    next_stage_ready = tracker.next_stage_ready,
                    "[cook-progress] oven moved on"
                );
            }
            return;
        }

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
            self.preheat_tolerance_c,
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
    }

    /// Send the progress, when it changed. The handlers send once, at the
    /// end, so a reader never sees a new stage with the old stage's times.
    fn publish(&self) {
        let progress = self.tracker.as_ref().map(|t| CookProgress {
            stage_started_at: self.times.started_at,
            stage_ends_at: self.times.end.ends_at(),
            ..build_cook_progress(t)
        });
        let _ = self.progress_tx.send_if_modified(|current| {
            let changed = *current != progress;
            if changed {
                *current = progress;
            }
            changed
        });
    }
}

/// Take the stage and `next_stage_ready` from the oven's own state. A set
/// `stageTransitionPendingUserAction` on the last stage means the cook is
/// done (or `START_NEXT_STAGE` was sent on it), not that a stage waits, so
/// it only counts with a stage still to come.
///
/// A cook stage with neither timer nor probe ("when I say") never ends, so
/// the oven never sets the flag on it: it holds until the next stage is
/// started. That is the oven waiting for the user too, from the moment the
/// stage starts. Returns whether anything changed.
fn follow_oven(tracker: &mut Tracker, status: &OvenStatus) -> bool {
    let before = (tracker.current_stage_index, tracker.next_stage_ready);
    if let Some(index) = status
        .active_stage_index
        .filter(|i| *i < tracker.stages.len())
    {
        if index != tracker.current_stage_index {
            tracker.advance_to(index);
        }
    }
    let holds = tracker.current_stage().is_some_and(holds_until_told);
    tracker.next_stage_ready =
        (status.stage_transition_pending_user_action || holds) && tracker.next_stage().is_some();
    before != (tracker.current_stage_index, tracker.next_stage_ready)
}

/// A cook stage that only ends when the next one is started.
fn holds_until_told(stage: &Stage) -> bool {
    stage.kind != "preheat" && stage.timer_added != Some(true) && stage.probe_added != Some(true)
}

fn build_cook_progress(tracker: &Tracker) -> CookProgress {
    let current = tracker.current_stage();
    let current_description = current
        .map(Stage::short_description)
        .unwrap_or_else(|| "Unknown stage".into());
    let current_kind = current
        .map(|s| s.kind.clone())
        .unwrap_or_else(|| "cook".into());
    let next = tracker.next_stage().filter(|_| tracker.next_stage_ready);
    let steps = anova_oven_api::steps(&tracker.stages);

    CookProgress {
        recipe_title: tracker.recipe_title.clone(),
        current_stage_index: tracker.current_stage_index,
        total_stage_count: tracker.stages.len(),
        current_stage_description: current_description,
        current_stage_kind: current_kind,
        next_stage_ready: tracker.next_stage_ready,
        next_stage_description: next.map(Stage::short_description),
        next_stage_prompt: next.and_then(|s| s.prompt.clone()),
        current_step_index: steps
            .iter()
            .position(|r| r.contains(&tracker.current_stage_index))
            .unwrap_or(0),
        step_count: steps.len(),
        // Set by the task, which keeps the clocks.
        stage_started_at: None,
        stage_ends_at: None,
    }
}

/// Evaluate the three completion heuristics (timer, probe, preheat + hold)
/// for `stage` given the latest `status`. Mutates `preheat_cross_since` to
/// track the preheat stability window. A preheat within
/// `preheat_tolerance_c` of its target counts as having crossed it.
fn evaluate_stage_completion(
    stage: &Stage,
    status: &OvenStatus,
    preheat_cross_since: &mut Option<Instant>,
    warned_no_criterion: &mut bool,
    stage_index: usize,
    timer_has_run: &mut bool,
    preheat_tolerance_c: f32,
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
    //
    // NOTE: `timer_current_secs` counts *down* (see `OvenStatus`), so this
    // fires on the oven parking the timer back at its full duration once the
    // stage ends — not on a count-up reaching it, which is what this condition
    // was written for. The `timer_has_run` guard above is what keeps the
    // identical reading at stage *start* from firing it. It has held up in
    // practice, but it is testing a side effect: if a stage ever stops
    // completing, check whether that oven leaves `current` at 0 instead, and
    // key this off `timer_mode` leaving `"running"` after having run.
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
            let hysteresis_floor = target - preheat_tolerance_c.max(PREHEAT_HYSTERESIS_C);
            match *preheat_cross_since {
                None if current >= target - preheat_tolerance_c => {
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
pub(crate) mod tests {
    use super::{
        arrival_from, evaluate_stage_completion, Arrival, CookProgressTask, StageEnd, StageKey,
        StageTimes,
    };
    use anova_oven_api::{CookProgress, CurrentCook, OvenStatus, Stage};
    use tokio::sync::watch;

    fn task_with_rx() -> (CookProgressTask, watch::Receiver<Option<CookProgress>>) {
        let (tx, rx) = watch::channel(None);
        (CookProgressTask::new(tx, false, 0.0), rx)
    }

    fn local_task_with_rx() -> (CookProgressTask, watch::Receiver<Option<CookProgress>>) {
        let (tx, rx) = watch::channel(None);
        (CookProgressTask::new(tx, true, 0.0), rx)
    }

    fn oven_status(active: usize, pending: bool) -> OvenStatus {
        OvenStatus {
            active_stage_index: Some(active),
            stage_transition_pending_user_action: pending,
            ..base_status()
        }
    }

    fn progress(rx: &watch::Receiver<Option<CookProgress>>) -> (usize, bool) {
        let p = rx.borrow().clone().expect("progress");
        (p.current_stage_index, p.next_stage_ready)
    }

    /// Three stages that each end on a timer.
    fn timed_cook() -> CurrentCook {
        let mut cook = stale_cook();
        for stage in &mut cook.stages {
            stage.timer_added = Some(true);
            stage.duration_secs = Some(600);
        }
        cook
    }

    #[test]
    fn local_mode_follows_the_ovens_stage_and_waiting_flag() {
        let (mut task, rx) = local_task_with_rx();
        let status = oven_status(0, false);
        task.on_current_cook(Some(timed_cook()), Some(&status));
        task.on_status(Some(&status));
        assert_eq!(progress(&rx), (0, false));

        // Stage 0 done; stage 1 waits for the user.
        task.on_status(Some(&oven_status(0, true)));
        assert_eq!(progress(&rx), (0, true));

        // The user started it.
        task.on_status(Some(&oven_status(1, false)));
        assert_eq!(progress(&rx), (1, false));
    }

    #[test]
    fn a_waiting_stage_passes_on_its_prompt() {
        let (mut task, rx) = local_task_with_rx();
        let mut cook = timed_cook();
        cook.stages[1].prompt = Some("Rotate the pan".into());
        task.on_current_cook(Some(cook), Some(&oven_status(0, false)));
        let prompt = |rx: &watch::Receiver<Option<CookProgress>>| {
            rx.borrow().clone().and_then(|p| p.next_stage_prompt)
        };
        assert_eq!(prompt(&rx), None, "not until the oven waits");
        task.on_status(Some(&oven_status(0, true)));
        assert_eq!(prompt(&rx).as_deref(), Some("Rotate the pan"));
        task.on_status(Some(&oven_status(1, false)));
        assert_eq!(prompt(&rx), None);
    }

    #[test]
    fn local_mode_waiting_flag_on_the_last_stage_is_not_a_next_stage() {
        let (mut task, rx) = local_task_with_rx();
        task.on_current_cook(Some(timed_cook()), Some(&oven_status(2, false)));
        task.on_status(Some(&oven_status(2, true)));
        assert_eq!(progress(&rx), (2, false));
        // Nor is a last stage that holds until told.
        let (mut task, rx) = local_task_with_rx();
        task.on_current_cook(Some(stale_cook()), Some(&oven_status(2, false)));
        assert_eq!(progress(&rx), (2, false));
    }

    #[test]
    fn a_stage_that_holds_until_told_waits_for_the_user_from_its_start() {
        let (mut task, rx) = local_task_with_rx();
        let mut cook = timed_cook();
        cook.stages[0].kind = "preheat".into();
        cook.stages[0].timer_added = Some(false);
        cook.stages[1].timer_added = Some(false);
        cook.stages[2].prompt = Some("Load the loaf".into());
        task.on_current_cook(Some(cook), Some(&oven_status(0, false)));
        assert_eq!(progress(&rx), (0, false), "a preheat ends by itself");

        // Preheated: the hold starts, and the oven never says it waits.
        task.on_status(Some(&oven_status(1, false)));
        assert_eq!(progress(&rx), (1, true));
        let prompt = rx.borrow().clone().and_then(|p| p.next_stage_prompt);
        assert_eq!(prompt.as_deref(), Some("Load the loaf"));

        task.on_status(Some(&oven_status(2, false)));
        assert_eq!(progress(&rx), (2, false));
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
            turn_off_at_end: None,
            prompt: None,
        }
    }

    /// A cooking oven at 25 °C. Shared with other modules' tests.
    pub(crate) fn base_status() -> OvenStatus {
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
            probe_target_temperature_c: None,
            wet_target_temperature_c: None,
            timer_current_secs: 0,
            timer_total_secs: 0,
            timer_mode: "idle".into(),
            steam_pct: 0.0,
            steam_target_pct: Some(0.0),
            steam_generator_mode: "idle".into(),
            boiler_celsius: 0.0,
            boiler_watts: 0.0,
            boiler_descale_required: false,
            faults: Default::default(),
            wet_bulb_dose_failed: false,
            firmware_version: None,
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
            cook_done: false,
            cook_id: None,
            stage_transition_pending_user_action: false,
            cook_progress: None,
            upstream: None,
            scheduled_start: None,
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
            0.0,
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
            0.0,
        );

        assert!(!complete);
    }

    #[test]
    fn a_preheat_within_tolerance_counts_as_crossing_its_target() {
        let mut stage = base_stage();
        stage.kind = "preheat".into();
        stage.temperature_c = 32.0;
        stage.duration_secs = None;
        stage.timer_added = None;
        stage.probe_added = None;
        stage.probe_target_c = None;
        let mut status = base_status();
        status.temperature_bulbs_mode = "dry".into();
        status.temperature_c = 31.0;

        let check = |tolerance, since: &mut Option<std::time::Instant>| {
            evaluate_stage_completion(&stage, &status, since, &mut false, 0, &mut false, tolerance)
        };
        let mut since = None;
        check(0.0, &mut since);
        assert!(
            since.is_none(),
            "1 °C short isn't there without a tolerance"
        );
        check(1.1, &mut since);
        assert!(since.is_some(), "but is within 2 °F");
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

    fn key(cook: Option<&str>, index: Option<usize>) -> Option<StageKey> {
        Some(StageKey {
            cook_id: cook.map(String::from),
            stage_index: index,
        })
    }

    fn seen(at: i64) -> Arrival {
        Arrival { at, watched: true }
    }

    fn unseen(at: i64) -> Arrival {
        Arrival { at, watched: false }
    }

    #[test]
    fn an_unknown_part_is_no_new_cook_or_stage() {
        let a0 = key(Some("a"), Some(0)).unwrap();
        let unknown = StageKey::default();
        assert!(!a0.new_cook(&unknown) && !unknown.new_cook(&a0));
        assert!(!a0.new_stage(&unknown) && !unknown.new_stage(&a0));
        let b1 = key(Some("b"), Some(1)).unwrap();
        assert!(a0.new_cook(&b1) && a0.new_stage(&b1));
    }

    #[test]
    fn a_gap_of_over_a_minute_is_not_watched() {
        let a = arrival_from(Some(100_000), Some(95_000), 0);
        assert_eq!((a.at, a.watched), (100, true));
        assert!(!arrival_from(Some(200_000), Some(100_000), 0).watched);
        // The first report since the server started.
        assert!(!arrival_from(Some(100_000), None, 0).watched);
        let a = arrival_from(None, None, 7);
        assert_eq!((a.at, a.watched), (7, false));
    }

    #[test]
    fn a_stage_starts_when_the_server_sees_it_start() {
        let mut t = StageTimes::default();
        t.update(None, None, 0, seen(100));
        assert_eq!(t.started_at, None);
        // The cook starts; its ID arrives a moment later: the same stage.
        t.update(key(None, Some(0)), None, 0, seen(110));
        t.update(key(Some("a"), Some(0)), None, 0, seen(115));
        assert_eq!(t.started_at, Some(110));
        t.update(key(Some("a"), Some(1)), None, 0, seen(400));
        t.update(key(Some("a"), Some(1)), None, 0, seen(900));
        assert_eq!(t.started_at, Some(400));
        t.update(None, None, 0, seen(950));
        assert_eq!(t.started_at, None);
    }

    #[test]
    fn a_start_the_server_did_not_see_is_none() {
        // The server's first report is mid-cook.
        let mut t = StageTimes::default();
        t.update(key(Some("a"), Some(1)), None, 0, unseen(100));
        t.update(key(Some("a"), Some(1)), None, 0, seen(105));
        assert_eq!(t.started_at, None);

        // The oven was away, and came back in the next stage.
        let mut t = StageTimes::default();
        t.update(None, None, 0, seen(0));
        t.update(key(Some("a"), Some(0)), None, 0, seen(10));
        assert_eq!(t.started_at, Some(10));
        t.update(key(Some("a"), Some(1)), None, 0, unseen(3000));
        assert_eq!(t.started_at, None);
        // The stage after that is seen again.
        t.update(key(Some("a"), Some(2)), None, 0, seen(3600));
        assert_eq!(t.started_at, Some(3600));
    }

    #[test]
    fn a_stage_index_that_arrives_late_is_no_new_stage() {
        let mut t = StageTimes::default();
        t.update(key(Some("a"), None), None, 0, unseen(100));
        t.update(key(Some("a"), Some(1)), None, 0, seen(105));
        assert_eq!(t.started_at, None);
    }

    #[test]
    fn a_cook_id_that_goes_away_and_comes_back_is_the_same_cook() {
        let mut t = StageTimes::default();
        t.update(None, None, 0, seen(0));
        t.update(key(Some("a"), Some(0)), None, 0, seen(10));
        t.update(key(None, Some(0)), None, 0, seen(15));
        t.update(key(Some("a"), Some(0)), None, 0, seen(20));
        assert_eq!(t.started_at, Some(10));
    }

    #[test]
    fn the_end_stays_put_between_reports() {
        let mut end = StageEnd::default();
        end.update(Some(600), 900, 1000);
        assert_eq!(end.ends_at(), Some(1600));
        // The next report, 5 s later.
        end.update(Some(595), 900, 1005);
        assert_eq!(end.ends_at(), Some(1600));
        // Whole seconds: the same end works out a second later.
        end.update(Some(589), 900, 1012);
        assert_eq!(end.ends_at(), Some(1600));
        // The timer set again: a new total, a new end.
        end.update(Some(1200), 1200, 1013);
        assert_eq!(end.ends_at(), Some(2213));
        // No running timer: no end.
        end.update(None, 0, 1014);
        assert_eq!(end.ends_at(), None);
    }

    #[test]
    fn the_progress_carries_the_stage_times() {
        let (mut task, rx) = local_task_with_rx();
        task.on_status_at(Some(&idle_status()), seen(100));
        let mut status = oven_status(0, false);
        status.cook_id = Some("a".into());
        task.on_status_at(Some(&status), seen(110));
        task.on_current_cook(Some(timed_cook()), Some(&status));
        let p = rx.borrow().clone().expect("progress");
        assert_eq!(p.stage_started_at, Some(110));
        assert_eq!(p.stage_ends_at, None);

        let mut status = oven_status(1, false);
        status.cook_id = Some("a".into());
        status.timer_mode = "running".into();
        status.timer_total_secs = 600;
        status.timer_current_secs = 600;
        task.on_status_at(Some(&status), seen(400));
        let p = rx.borrow().clone().expect("progress");
        assert_eq!(p.current_stage_index, 1);
        assert_eq!(p.stage_started_at, Some(400));
        assert_eq!(p.stage_ends_at, Some(1000));
    }
}
