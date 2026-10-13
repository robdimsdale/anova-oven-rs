//! Prometheus metrics for `GET /metrics`. See `docs/monitoring.md`, Phase 1.
//!
//! Instrumentation sites call the `metrics` facade macros directly with the
//! name constants below; this module owns the recorder, the `# HELP` text,
//! the histogram buckets, and the metrics derived from shared state rather
//! than from events (upstream liveness, the latest `OvenStatus`).
//!
//! Every label value is drawn from a closed set: never put a recipe title,
//! recipe ID, cook ID or timestamp in a label, because each distinct value
//! creates a permanent new series.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anova_oven_api::{CookProgress, OvenStatus};
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, Unit};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tokio::sync::watch;

use crate::liveness::Liveness;
use crate::protocol::SystemInfo;

pub const UPSTREAM_CONNECTED: &str = "anova_upstream_connected";
pub const UPSTREAM_LAST_STATE_TIMESTAMP: &str = "anova_upstream_last_state_timestamp_seconds";
pub const WS_FRAMES: &str = "anova_ws_frames_total";
pub const WS_RECONNECTS: &str = "anova_ws_reconnects_total";
pub const FIRESTORE_REQUESTS: &str = "anova_firestore_requests_total";
pub const FIRESTORE_REQUEST_DURATION: &str = "anova_firestore_request_duration_seconds";
pub const HTTP_REQUESTS: &str = "anova_http_requests_total";
pub const PROCESS_START_TIME: &str = "anova_process_start_time_seconds";
pub const OVEN_TEMP: &str = "anova_oven_temp_celsius";
pub const OVEN_ELEMENT_WATTS: &str = "anova_oven_element_watts";
pub const OVEN_COOK_ACTIVE: &str = "anova_oven_cook_active";
pub const OVEN_DOOR_OPEN: &str = "anova_oven_door_open";
pub const OVEN_WATER_TANK_EMPTY: &str = "anova_oven_water_tank_empty";
pub const OVEN_ELEMENT_ENERGY: &str = "anova_oven_element_energy_joules_total";
pub const OVEN_TARGET_TEMP: &str = "anova_oven_target_temp_celsius";
pub const OVEN_HUMIDITY: &str = "anova_oven_humidity_percent";
pub const OVEN_TARGET_HUMIDITY: &str = "anova_oven_target_humidity_percent";
pub const OVEN_FAN_SPEED: &str = "anova_oven_fan_speed_percent";
pub const OVEN_MODE: &str = "anova_oven_mode";
pub const OVEN_BULB_MODE: &str = "anova_oven_bulb_mode";
pub const OVEN_TIMER_REMAINING: &str = "anova_oven_timer_remaining_seconds";
pub const OVEN_TIMER_TOTAL: &str = "anova_oven_timer_total_seconds";
pub const OVEN_TIMER_RUNNING: &str = "anova_oven_timer_running";
pub const OVEN_PROBE_CONNECTED: &str = "anova_oven_probe_connected";
pub const OVEN_VENT_OPEN: &str = "anova_oven_vent_open";
pub const OVEN_LAMP_ON: &str = "anova_oven_lamp_on";
pub const OVEN_DESCALE_REQUIRED: &str = "anova_oven_descale_required";
pub const OVEN_ONLINE: &str = "anova_oven_online";
pub const OVEN_INFO: &str = "anova_oven_info";

// Rendered by hand in [`CookTimeline::render`], not through the recorder:
// these series must vanish when the cook ends, and the recorder keeps every
// series it has ever seen.
pub const COOK_INFO: &str = "anova_cook_info";
pub const COOK_STAGE_COUNT: &str = "anova_cook_stage_count";
pub const COOK_CURRENT_STAGE: &str = "anova_cook_current_stage";
pub const COOK_STAGE_START_TIME: &str = "anova_cook_stage_start_time_seconds";

/// `state.mode` values with their own series; anything else is `other`.
const OVEN_MODES: [&str; 3] = ["idle", "cook", "preheat"];
/// `temperatureBulbs.mode` values.
const BULB_MODES: [&str; 2] = ["dry", "wet"];

/// Longest gap between two oven-state frames that energy is integrated
/// across. Anova sends a frame every ~10 s while cooking, so a longer gap
/// mid-cook means the link dropped and the watts in hand are stale; the
/// interval is skipped rather than guessed. Idle gaps (~10 min) are skipped
/// too, which costs nothing since idle watts are zero.
const MAX_ENERGY_GAP_MS: u64 = 60_000;

/// `Content-Type` for the Prometheus text exposition format. `0.0.4` is that
/// format's version (unchanged since Prometheus 0.4); the `version` parameter
/// is how a scraper tells it apart from OpenMetrics or protobuf.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Firestore calls are HTTPS round-trips to Google with retries and a 15s
/// outer timeout, so the interesting range is ~50ms to ~15s.
const FIRESTORE_DURATION_BUCKETS: &[f64] = &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 15.0];

/// Histograms accumulate until drained; `install_recorder` (unlike the
/// crate's bundled HTTP listener) does not run upkeep for us.
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Install the global recorder, describe every metric, stamp the process
/// start time, and start the upkeep task. Call once, from inside the tokio
/// runtime, before any instrumented code runs.
pub fn install() -> PrometheusHandle {
    let handle = builder()
        .install_recorder()
        .expect("failed to install Prometheus recorder");
    describe();

    // How supervisor restarts become visible: `main` exits the process when a
    // critical task dies and systemd restarts it, which is otherwise silent.
    // `changes(anova_process_start_time_seconds[1h])` counts them.
    gauge!(PROCESS_START_TIME).set(unix_secs(SystemTime::now()));

    let upkeep = handle.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(UPKEEP_INTERVAL);
        loop {
            interval.tick().await;
            upkeep.run_upkeep();
        }
    });

    handle
}

fn builder() -> PrometheusBuilder {
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full(FIRESTORE_REQUEST_DURATION.to_string()),
            FIRESTORE_DURATION_BUCKETS,
        )
        .expect("bucket list is non-empty")
}

fn describe() {
    describe_gauge!(
        UPSTREAM_CONNECTED,
        "1 if the WebSocket to Anova is currently connected."
    );
    describe_gauge!(
        UPSTREAM_LAST_STATE_TIMESTAMP,
        Unit::Seconds,
        "Unix time of the last oven-state frame from Anova. Absent until the first one."
    );
    describe_counter!(WS_FRAMES, "WebSocket frames received from Anova, by kind.");
    describe_counter!(
        WS_RECONNECTS,
        "Times the Anova WebSocket connection ended and a reconnect was scheduled."
    );
    describe_counter!(
        FIRESTORE_REQUESTS,
        "Firestore operations, by operation and outcome (including retries)."
    );
    describe_histogram!(
        FIRESTORE_REQUEST_DURATION,
        Unit::Seconds,
        "Firestore operation latency, including retries and session refresh."
    );
    describe_counter!(HTTP_REQUESTS, "HTTP requests served, by route and status.");
    describe_gauge!(
        PROCESS_START_TIME,
        Unit::Seconds,
        "Unix time this server process started."
    );
    describe_gauge!(
        OVEN_TEMP,
        "Oven temperatures in Celsius, by bulb. NaN while the probe is unplugged."
    );
    describe_gauge!(OVEN_ELEMENT_WATTS, "Power draw in watts, by element.");
    describe_gauge!(
        OVEN_COOK_ACTIVE,
        "1 while the oven is cooking or preheating."
    );
    describe_gauge!(OVEN_DOOR_OPEN, "1 while the oven door is open.");
    describe_gauge!(OVEN_WATER_TANK_EMPTY, "1 while the water tank is empty.");
    describe_counter!(
        OVEN_ELEMENT_ENERGY,
        "Energy drawn in joules, by element, integrated across oven-state frames."
    );
    describe_gauge!(
        OVEN_TARGET_TEMP,
        "Target temperature in Celsius, by bulb (dry, wet, probe). NaN when that bulb has no target."
    );
    describe_gauge!(OVEN_HUMIDITY, "Relative humidity in the cavity, percent.");
    describe_gauge!(
        OVEN_TARGET_HUMIDITY,
        "Target relative humidity, percent. NaN when the stage sets none."
    );
    describe_gauge!(OVEN_FAN_SPEED, "Convection fan speed, percent.");
    describe_gauge!(
        OVEN_MODE,
        "1 for the oven's reported state.mode, 0 for the others."
    );
    describe_gauge!(
        OVEN_BULB_MODE,
        "1 for the bulb the oven regulates on (dry, or wet for sous vide), 0 for the other."
    );
    describe_gauge!(
        OVEN_TIMER_REMAINING,
        Unit::Seconds,
        "Seconds left on the stage timer (the oven counts down). Equals the total before it starts."
    );
    describe_gauge!(
        OVEN_TIMER_TOTAL,
        Unit::Seconds,
        "Stage timer duration in seconds."
    );
    describe_gauge!(
        OVEN_TIMER_RUNNING,
        "1 while the stage timer is counting down."
    );
    describe_gauge!(
        OVEN_PROBE_CONNECTED,
        "1 while the meat probe is plugged in."
    );
    describe_gauge!(OVEN_VENT_OPEN, "1 while the vent is open.");
    describe_gauge!(OVEN_LAMP_ON, "1 while the oven lamp is on.");
    describe_gauge!(
        OVEN_DESCALE_REQUIRED,
        "1 while the boiler reports it needs descaling."
    );
    describe_gauge!(
        OVEN_ONLINE,
        "1 while Anova's cloud sees the oven as connected (systemInfo.online)."
    );
    describe_gauge!(
        OVEN_INFO,
        "Oven firmware and hardware versions as labels. 1 for the current pair, 0 for pairs seen earlier in this process."
    );
}

/// Set the gauges derived from upstream liveness. Called at scrape time so
/// they are exactly current rather than tracking every WebSocket event.
///
/// The last-state gauge is an absolute timestamp, not an age: an age computed
/// at scrape time would freeze in the store if scraping stopped. PromQL
/// computes `time() - anova_upstream_last_state_timestamp_seconds` instead.
pub fn record_liveness(liveness: &Liveness) {
    gauge!(UPSTREAM_CONNECTED).set(f64::from(u8::from(liveness.connected())));
    if let Some(ms) = liveness.last_state_unix_ms() {
        gauge!(UPSTREAM_LAST_STATE_TIMESTAMP).set(ms as f64 / 1000.0);
    }
}

/// Set the oven-domain gauges from one status reading.
pub fn record_oven_status(status: &OvenStatus) {
    for (bulb, celsius) in [
        ("dry", status.temperature_c),
        ("dry_top", status.dry_top_temperature_c),
        ("dry_bottom", status.dry_bottom_temperature_c),
        ("wet", status.wet_bulb_temperature_c),
        ("probe", status.probe_temperature_c.unwrap_or(f32::NAN)),
        ("boiler", status.boiler_celsius),
        ("evaporator", status.evaporator_celsius),
    ] {
        gauge!(OVEN_TEMP, "bulb" => bulb).set(f64::from(celsius));
    }

    for (element, watts) in [
        ("top", status.heating_element_top_watts),
        ("rear", status.heating_element_rear_watts),
        ("bottom", status.heating_element_bottom_watts),
        ("boiler", status.boiler_watts),
        ("evaporator", status.evaporator_watts),
    ] {
        gauge!(OVEN_ELEMENT_WATTS, "element" => element).set(f64::from(watts));
    }

    // Every bulb is set on every frame, NaN when it has no target, so a
    // target from an earlier stage never lingers as a live-looking value.
    for (bulb, celsius) in [
        ("dry", status.target_temperature_c),
        ("wet", status.wet_target_temperature_c),
        ("probe", status.probe_target_temperature_c),
    ] {
        gauge!(OVEN_TARGET_TEMP, "bulb" => bulb).set(opt_gauge(celsius));
    }
    gauge!(OVEN_HUMIDITY).set(f64::from(status.steam_pct));
    gauge!(OVEN_TARGET_HUMIDITY).set(opt_gauge(status.steam_target_pct));
    gauge!(OVEN_FAN_SPEED).set(f64::from(status.fan_speed));

    set_one_hot(OVEN_MODE, "mode", &OVEN_MODES, &status.mode);
    set_one_hot(
        OVEN_BULB_MODE,
        "mode",
        &BULB_MODES,
        &status.temperature_bulbs_mode,
    );

    gauge!(OVEN_TIMER_REMAINING).set(status.timer_current_secs as f64);
    gauge!(OVEN_TIMER_TOTAL).set(status.timer_total_secs as f64);
    gauge!(OVEN_TIMER_RUNNING).set(bool_gauge(status.timer_mode == "running"));

    gauge!(OVEN_COOK_ACTIVE).set(bool_gauge(status.mode != "idle"));
    gauge!(OVEN_DOOR_OPEN).set(bool_gauge(status.door_open));
    gauge!(OVEN_WATER_TANK_EMPTY).set(bool_gauge(status.water_tank_empty));
    gauge!(OVEN_PROBE_CONNECTED).set(bool_gauge(status.probe_temperature_c.is_some()));
    gauge!(OVEN_VENT_OPEN).set(bool_gauge(status.vent_open));
    gauge!(OVEN_LAMP_ON).set(bool_gauge(status.lamp_on));
    gauge!(OVEN_DESCALE_REQUIRED).set(bool_gauge(status.boiler_descale_required));
}

/// Set `name{label=v}` to 1 for the `value` in `known` and 0 for the rest,
/// with `other` catching anything outside the closed set, so a new upstream
/// value never mints a series of its own.
fn set_one_hot(name: &'static str, label: &'static str, known: &[&'static str], value: &str) {
    let current = known
        .iter()
        .copied()
        .find(|k| *k == value)
        .unwrap_or("other");
    for k in known.iter().copied().chain(["other"]) {
        gauge!(name, label => k).set(bool_gauge(k == current));
    }
}

/// Record the oven's `systemInfo`, straight from each state frame (it is not
/// part of `OvenStatus`, which the panels parse every second).
///
/// The info series follows the usual `_info` pattern, except that the
/// recorder never drops a series: after a firmware update the old pair stays
/// at 0, so filter on `anova_oven_info == 1`.
pub fn record_system_info(info: &SystemInfo) {
    if let Some(online) = info.online {
        gauge!(OVEN_ONLINE).set(bool_gauge(online));
    }
    let (Some(firmware), Some(hardware)) = (&info.firmware_version, &info.hardware_version) else {
        return;
    };
    let current = (firmware.clone(), hardware.clone());
    let mut last = LAST_INFO.lock().expect("info lock poisoned");
    if last.as_ref() == Some(&current) {
        return;
    }
    if let Some((firmware, hardware)) = last.take() {
        gauge!(OVEN_INFO, "firmware" => firmware, "hardware" => hardware).set(0.0);
    }
    gauge!(OVEN_INFO, "firmware" => current.0.clone(), "hardware" => current.1.clone()).set(1.0);
    *last = Some(current);
}

static LAST_INFO: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Mirror the read model's latest `OvenStatus` into the oven gauges and the
/// energy counters for the life of the process. Gauges keep their last value
/// while upstream is down; staleness is what
/// `anova_upstream_last_state_timestamp_seconds` is for.
pub async fn run_oven_status_gauges(
    mut status_rx: watch::Receiver<Option<OvenStatus>>,
    liveness: Arc<Liveness>,
) {
    let mut energy = EnergyIntegrator::default();
    loop {
        if let Some(status) = status_rx.borrow_and_update().as_ref() {
            record_oven_status(status);
            // The status channel also fires on read-model changes that are not
            // new frames (a Firestore refresh, say), including while upstream
            // is down. Integrating only on a new frame timestamp keeps a stale
            // reading from being counted across an outage.
            if let Some(frame_ms) = liveness.last_state_unix_ms() {
                if energy.observe(frame_ms, element_watts(status)) {
                    for (element, joules) in ELEMENTS.iter().zip(energy.joules) {
                        counter!(OVEN_ELEMENT_ENERGY, "element" => *element)
                            .absolute(joules as u64);
                    }
                }
            }
        }
        if status_rx.changed().await.is_err() {
            return;
        }
    }
}

const ELEMENTS: [&str; 5] = ["top", "rear", "bottom", "boiler", "evaporator"];

fn element_watts(status: &OvenStatus) -> [f32; 5] {
    [
        status.heating_element_top_watts,
        status.heating_element_rear_watts,
        status.heating_element_bottom_watts,
        status.boiler_watts,
        status.evaporator_watts,
    ]
}

/// Running energy totals per element, in [`ELEMENTS`] order.
///
/// Watts arrive as ~10 s snapshots and the elements cycle faster than any
/// scrape, so a `_watts` gauge sampled every few seconds is a random draw.
/// A counter integrated on every frame keeps what happened between scrapes:
/// `rate(anova_oven_element_energy_joules_total[1m])` is the true mean power
/// over that minute, as far as the frames show it. Each frame's watts are
/// held until the next frame (a left Riemann sum), which is what the oven
/// reported for that interval.
#[derive(Default)]
struct EnergyIntegrator {
    joules: [f64; 5],
    last: Option<(u64, [f32; 5])>,
}

impl EnergyIntegrator {
    /// Account for a frame at `frame_ms`. Returns `true` when the totals
    /// changed. A repeated timestamp is the same frame seen again and is
    /// ignored.
    fn observe(&mut self, frame_ms: u64, watts: [f32; 5]) -> bool {
        let mut changed = false;
        match self.last {
            Some((prev_ms, _)) if frame_ms <= prev_ms => return false,
            Some((prev_ms, prev_watts)) if frame_ms - prev_ms <= MAX_ENERGY_GAP_MS => {
                let secs = (frame_ms - prev_ms) as f64 / 1000.0;
                for (total, w) in self.joules.iter_mut().zip(prev_watts) {
                    *total += f64::from(w.max(0.0)) * secs;
                }
                changed = true;
            }
            _ => {}
        }
        self.last = Some((frame_ms, watts));
        changed
    }
}

/// The current cook, as hand-rendered series that exist only while it runs:
///
/// * `anova_cook_info{recipe}` — which recipe this is. The one place recipe
///   identity appears; join to it at query time rather than labelling the
///   measurements (see "Cardinality" in `docs/monitoring.md`).
/// * `anova_cook_stage_count` and `anova_cook_current_stage{index,kind}`.
/// * `anova_cook_stage_start_time_seconds{index,kind}` — when each stage
///   started, as the sample *value*, so it is exact however coarsely the
///   endpoint is scraped: a one-minute stage that begins and ends between two
///   scrapes is still there, with its real start, on the next one.
///
/// When the cook ends every one of these disappears (and the scraper marks
/// them stale), which is why they bypass the recorder.
#[derive(Default)]
pub struct CookTimeline {
    cook: Mutex<Option<CookRecord>>,
}

#[derive(Debug, Clone, PartialEq)]
struct CookRecord {
    recipe_title: String,
    stage_count: usize,
    stages: Vec<StageStart>,
}

#[derive(Debug, Clone, PartialEq)]
struct StageStart {
    index: usize,
    kind: &'static str,
    unix_secs: f64,
}

impl CookTimeline {
    /// Fold one `CookProgress` update in, stamping a newly seen stage with
    /// `now_unix_secs`.
    ///
    /// A stage is timed from when this server first saw it, so a stage
    /// already running when the server started reads as starting then.
    fn observe(&self, progress: Option<&CookProgress>, now_unix_secs: f64) {
        let mut cook = self.cook.lock().expect("timeline lock poisoned");
        let Some(progress) = progress else {
            *cook = None;
            return;
        };
        let index = progress.current_stage_index;
        // Stages only move forward within a cook, so a lower index is a new
        // cook that arrived without an idle update in between.
        if cook
            .as_ref()
            .and_then(|c| c.stages.last())
            .is_some_and(|last| last.index > index)
        {
            *cook = None;
        }
        let record = cook.get_or_insert_with(|| CookRecord {
            recipe_title: String::new(),
            stage_count: 0,
            stages: Vec::new(),
        });
        // Title and count can firm up mid-cook (a manual cook resolved to a
        // recipe, a Firestore refresh), so they follow the latest update.
        record.recipe_title.clone_from(&progress.recipe_title);
        record.stage_count = progress.total_stage_count;
        if record.stages.last().is_some_and(|last| last.index == index) {
            return;
        }
        record.stages.push(StageStart {
            index,
            kind: stage_kind_label(&progress.current_stage_kind),
            unix_secs: now_unix_secs,
        });
    }

    /// Append the exposition text for the current cook to `out`. Writes
    /// nothing, not even `# HELP`, outside a cook.
    pub fn render(&self, out: &mut String) {
        let cook = self.cook.lock().expect("timeline lock poisoned");
        let Some(cook) = cook.as_ref() else {
            return;
        };
        let _ = writeln!(
            out,
            "# HELP {COOK_INFO} The running cook's recipe, as a label. Absent outside a cook.\n\
             # TYPE {COOK_INFO} gauge\n\
             {COOK_INFO}{{recipe=\"{}\"}} 1",
            LabelValue(&cook.recipe_title)
        );
        let _ = writeln!(
            out,
            "# HELP {COOK_STAGE_COUNT} Stages in the running cook, preheat included.\n\
             # TYPE {COOK_STAGE_COUNT} gauge\n\
             {COOK_STAGE_COUNT} {}",
            cook.stage_count
        );
        if let Some(current) = cook.stages.last() {
            let _ = writeln!(
                out,
                "# HELP {COOK_CURRENT_STAGE} The running cook's current stage, as labels.\n\
                 # TYPE {COOK_CURRENT_STAGE} gauge\n\
                 {COOK_CURRENT_STAGE}{{index=\"{}\",kind=\"{}\"}} 1",
                current.index, current.kind
            );
        }
        let _ = writeln!(
            out,
            "# HELP {COOK_STAGE_START_TIME} Unix time each stage of the running cook \
             started, as first seen by this server.\n\
             # TYPE {COOK_STAGE_START_TIME} gauge"
        );
        for s in &cook.stages {
            let _ = writeln!(
                out,
                "{COOK_STAGE_START_TIME}{{index=\"{}\",kind=\"{}\"}} {:.3}",
                s.index, s.kind, s.unix_secs
            );
        }
    }
}

/// A label value escaped for the text exposition format, which reserves
/// `\`, `"` and newlines. Recipe titles are free text.
struct LabelValue<'a>(&'a str);

impl std::fmt::Display for LabelValue<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for c in self.0.chars() {
            match c {
                '\\' => f.write_str("\\\\")?,
                '"' => f.write_str("\\\"")?,
                '\n' => f.write_str("\\n")?,
                c => f.write_char(c)?,
            }
        }
        Ok(())
    }
}

/// Stage kinds come from recipe data, so clamp them to a closed set before
/// they become a label value.
fn stage_kind_label(kind: &str) -> &'static str {
    match kind {
        "preheat" => "preheat",
        "cook" => "cook",
        _ => "other",
    }
}

/// Feed every `CookProgress` update into `timeline` for the life of the
/// process.
pub async fn run_cook_timeline(
    mut progress_rx: watch::Receiver<Option<CookProgress>>,
    timeline: Arc<CookTimeline>,
) {
    loop {
        timeline.observe(
            progress_rx.borrow_and_update().as_ref(),
            unix_secs(SystemTime::now()),
        );
        if progress_rx.changed().await.is_err() {
            return;
        }
    }
}

/// axum middleware counting every response by route and status. The route
/// label is the *matched* pattern, never the raw URI, so a scanner probing
/// random paths collapses into one `unmatched` series.
pub async fn track_http(req: Request, next: Next) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_string(), |p| p.as_str().to_string());
    let response = next.run(req).await;
    counter!(
        HTTP_REQUESTS,
        "route" => route,
        "status" => response.status().as_u16().to_string(),
    )
    .increment(1);
    response
}

fn opt_gauge(v: Option<f32>) -> f64 {
    v.map_or(f64::NAN, f64::from)
}

fn bool_gauge(b: bool) -> f64 {
    f64::from(u8::from(b))
}

fn unix_secs(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_with(f: impl FnOnce()) -> String {
        let recorder = builder().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, f);
        handle.render()
    }

    fn sample_status() -> OvenStatus {
        serde_json::from_value(serde_json::json!({
            "mode": "cook",
            "temperature_unit": "C",
            "temperature_c": 180.5,
            "target_temperature_c": 200.0,
            "temperature_bulbs_mode": "dry",
            "dry_top_temperature_c": 181.0,
            "dry_bottom_temperature_c": 179.0,
            "wet_bulb_temperature_c": 60.0,
            "timer_current_secs": 1500,
            "timer_total_secs": 1800,
            "timer_mode": "running",
            "steam_pct": 30.0,
            "steam_generator_mode": "idle",
            "boiler_celsius": 20.0,
            "boiler_watts": 0.0,
            "boiler_descale_required": false,
            "evaporator_celsius": 21.0,
            "evaporator_watts": 0.0,
            "fan_speed": 100,
            "heating_element_top_on": false,
            "heating_element_top_watts": 0.0,
            "heating_element_rear_on": true,
            "heating_element_rear_watts": 1200.0,
            "heating_element_bottom_on": false,
            "heating_element_bottom_watts": 0.0,
            "lamp_on": false,
            "lamp_preference": "off",
            "vent_open": false,
            "door_open": true,
            "water_tank_empty": false
        }))
        .expect("valid OvenStatus")
    }

    #[test]
    fn oven_status_renders_as_labelled_gauges() {
        let out = render_with(|| record_oven_status(&sample_status()));
        for line in [
            "anova_oven_temp_celsius{bulb=\"dry\"} 180.5",
            "anova_oven_temp_celsius{bulb=\"dry_bottom\"} 179",
            "anova_oven_temp_celsius{bulb=\"probe\"} NaN",
            "anova_oven_element_watts{element=\"rear\"} 1200",
            "anova_oven_cook_active 1",
            "anova_oven_door_open 1",
            "anova_oven_water_tank_empty 0",
            "anova_oven_target_temp_celsius{bulb=\"dry\"} 200",
            "anova_oven_target_temp_celsius{bulb=\"wet\"} NaN",
            "anova_oven_target_temp_celsius{bulb=\"probe\"} NaN",
            "anova_oven_humidity_percent 30",
            "anova_oven_target_humidity_percent NaN",
            "anova_oven_fan_speed_percent 100",
            "anova_oven_mode{mode=\"cook\"} 1",
            "anova_oven_mode{mode=\"idle\"} 0",
            "anova_oven_mode{mode=\"other\"} 0",
            "anova_oven_bulb_mode{mode=\"dry\"} 1",
            "anova_oven_bulb_mode{mode=\"wet\"} 0",
            "anova_oven_timer_remaining_seconds 1500",
            "anova_oven_timer_total_seconds 1800",
            "anova_oven_timer_running 1",
            "anova_oven_probe_connected 0",
            "anova_oven_vent_open 0",
            "anova_oven_lamp_on 0",
            "anova_oven_descale_required 0",
        ] {
            assert!(
                out.lines().any(|l| l == line),
                "missing `{line}` in:\n{out}"
            );
        }
    }

    #[test]
    fn idle_mode_is_not_an_active_cook() {
        let mut status = sample_status();
        status.mode = "idle".into();
        status.probe_temperature_c = Some(55.0);
        let out = render_with(|| record_oven_status(&status));
        assert!(out.lines().any(|l| l == "anova_oven_cook_active 0"));
        assert!(out.lines().any(|l| l == "anova_oven_mode{mode=\"idle\"} 1"));
        assert!(out.lines().any(|l| l == "anova_oven_probe_connected 1"));
        assert!(out
            .lines()
            .any(|l| l == "anova_oven_temp_celsius{bulb=\"probe\"} 55"));
    }

    #[test]
    fn liveness_omits_last_state_until_one_arrives() {
        let liveness = Liveness::new(60);
        let out = render_with(|| record_liveness(&liveness));
        assert!(out.lines().any(|l| l == "anova_upstream_connected 0"));
        assert!(!out.contains(UPSTREAM_LAST_STATE_TIMESTAMP));

        liveness.set_connected(true);
        liveness.record_state();
        let out = render_with(|| record_liveness(&liveness));
        assert!(out.lines().any(|l| l == "anova_upstream_connected 1"));
        let ts: f64 = out
            .lines()
            .find_map(|l| l.strip_prefix("anova_upstream_last_state_timestamp_seconds "))
            .expect("timestamp sample")
            .parse()
            .unwrap();
        let now = unix_secs(SystemTime::now());
        assert!(
            (now - ts).abs() < 5.0,
            "timestamp {ts} is not near now {now}"
        );
    }

    /// The route label must be the matched pattern (which requires the layer
    /// to sit where `MatchedPath` is visible) and unknown paths must collapse
    /// into a single series.
    #[tokio::test(flavor = "current_thread")]
    async fn http_requests_are_counted_by_matched_route() {
        let recorder = builder().build_recorder();
        let handle = recorder.handle();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let app = axum::Router::new()
            .route("/things/{id}", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(track_http));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        for path in ["/things/1", "/things/2", "/nope", "/also-nope"] {
            client
                .get(format!("http://{addr}{path}"))
                .send()
                .await
                .unwrap();
        }

        let out = handle.render();
        for line in [
            "anova_http_requests_total{route=\"/things/{id}\",status=\"200\"} 2",
            "anova_http_requests_total{route=\"unmatched\",status=\"404\"} 2",
        ] {
            assert!(
                out.lines().any(|l| l == line),
                "missing `{line}` in:\n{out}"
            );
        }
    }

    const REAR_ONLY: [f32; 5] = [0.0, 1200.0, 0.0, 0.0, 0.0];

    #[test]
    fn energy_holds_each_frames_watts_until_the_next_frame() {
        let mut e = EnergyIntegrator::default();
        assert!(!e.observe(1_000, REAR_ONLY), "first frame only anchors");
        assert!(e.observe(11_000, [0.0; 5]));
        assert_eq!(e.joules[1], 12_000.0, "1200 W held for 10 s");
        assert!(e.observe(21_000, REAR_ONLY));
        assert_eq!(e.joules[1], 12_000.0, "0 W held for the next 10 s");
    }

    #[test]
    fn energy_skips_repeated_frames_and_long_gaps() {
        let mut e = EnergyIntegrator::default();
        e.observe(1_000, REAR_ONLY);
        assert!(!e.observe(1_000, REAR_ONLY), "same frame seen again");
        // Upstream dropped mid-cook: the 1200 W reading is stale by now.
        assert!(!e.observe(1_000 + MAX_ENERGY_GAP_MS + 1, REAR_ONLY));
        assert_eq!(e.joules[1], 0.0);
        // Integration resumes from the frame after the gap.
        assert!(e.observe(1_000 + MAX_ENERGY_GAP_MS + 5_001, [0.0; 5]));
        assert_eq!(e.joules[1], 6_000.0);
    }

    fn progress(index: usize, kind: &str) -> CookProgress {
        CookProgress {
            recipe_title: "Roast Chicken".into(),
            current_stage_index: index,
            total_stage_count: 3,
            current_stage_description: String::new(),
            current_stage_kind: kind.into(),
            next_stage_ready: false,
            next_stage_description: None,
        }
    }

    fn rendered(t: &CookTimeline) -> String {
        let mut out = String::new();
        t.render(&mut out);
        out
    }

    #[test]
    fn timeline_keeps_every_stage_of_the_cook_with_its_start() {
        let t = CookTimeline::default();
        t.observe(Some(&progress(0, "preheat")), 100.0);
        t.observe(Some(&progress(0, "preheat")), 110.0);
        t.observe(Some(&progress(1, "cook")), 700.25);
        t.observe(Some(&progress(2, "cook")), 745.5);
        let out = rendered(&t);
        assert!(out.contains("# TYPE anova_cook_stage_start_time_seconds gauge\n"));
        for line in [
            "anova_cook_stage_start_time_seconds{index=\"0\",kind=\"preheat\"} 100.000",
            "anova_cook_stage_start_time_seconds{index=\"1\",kind=\"cook\"} 700.250",
            "anova_cook_stage_start_time_seconds{index=\"2\",kind=\"cook\"} 745.500",
        ] {
            assert!(
                out.lines().any(|l| l == line),
                "missing `{line}` in:\n{out}"
            );
        }
        for line in [
            "anova_cook_info{recipe=\"Roast Chicken\"} 1",
            "anova_cook_stage_count 3",
            "anova_cook_current_stage{index=\"2\",kind=\"cook\"} 1",
        ] {
            assert!(
                out.lines().any(|l| l == line),
                "missing `{line}` in:\n{out}"
            );
        }
        assert_eq!(
            out.lines()
                .filter(|l| l.starts_with(COOK_STAGE_START_TIME))
                .count(),
            3,
            "one start per stage:\n{out}"
        );
    }

    #[test]
    fn timeline_disappears_when_the_cook_ends() {
        let t = CookTimeline::default();
        t.observe(Some(&progress(0, "preheat")), 100.0);
        t.observe(None, 200.0);
        assert_eq!(rendered(&t), "");
    }

    #[test]
    fn timeline_restarts_on_a_lower_index_and_clamps_unknown_kinds() {
        let t = CookTimeline::default();
        t.observe(Some(&progress(2, "cook")), 100.0);
        t.observe(Some(&progress(0, "sear")), 200.0);
        let out = rendered(&t);
        assert!(out.lines().any(
            |l| l == "anova_cook_stage_start_time_seconds{index=\"0\",kind=\"other\"} 200.000"
        ));
        assert!(
            !out.contains("index=\"2\""),
            "old cook still listed:\n{out}"
        );
    }

    #[test]
    fn unknown_modes_fold_into_other() {
        let mut status = sample_status();
        status.mode = "descaling".into();
        status.temperature_bulbs_mode = "steam".into();
        let out = render_with(|| record_oven_status(&status));
        for line in [
            "anova_oven_mode{mode=\"other\"} 1",
            "anova_oven_mode{mode=\"cook\"} 0",
            "anova_oven_bulb_mode{mode=\"other\"} 1",
        ] {
            assert!(
                out.lines().any(|l| l == line),
                "missing `{line}` in:\n{out}"
            );
        }
        assert!(!out.contains("descaling") && !out.contains("steam\""));
    }

    #[test]
    fn recipe_titles_are_escaped() {
        let t = CookTimeline::default();
        let mut p = progress(0, "preheat");
        p.recipe_title = "Mum's \"best\" \\ roast\nv2".into();
        t.observe(Some(&p), 100.0);
        let out = rendered(&t);
        assert!(
            out.lines()
                .any(|l| l == r#"anova_cook_info{recipe="Mum's \"best\" \\ roast\nv2"} 1"#),
            "{out}"
        );
    }

    #[test]
    fn system_info_reports_online_and_retires_old_versions() {
        let info = |fw: &str| SystemInfo {
            online: Some(false),
            firmware_version: Some(fw.into()),
            hardware_version: Some("120V Universal".into()),
        };
        let out = render_with(|| {
            *LAST_INFO.lock().unwrap() = None;
            record_system_info(&info("2.1.7"));
            record_system_info(&info("2.1.8"));
        });
        for line in [
            "anova_oven_online 0",
            "anova_oven_info{firmware=\"2.1.7\",hardware=\"120V Universal\"} 0",
            "anova_oven_info{firmware=\"2.1.8\",hardware=\"120V Universal\"} 1",
        ] {
            assert!(
                out.lines().any(|l| l == line),
                "missing `{line}` in:\n{out}"
            );
        }
    }

    #[test]
    fn firestore_duration_is_a_histogram_with_buckets() {
        let out = render_with(|| {
            metrics::histogram!(FIRESTORE_REQUEST_DURATION, "op" => "recipes").record(0.3);
        });
        assert!(out.contains("# TYPE anova_firestore_request_duration_seconds histogram"));
        assert!(out.contains(
            "anova_firestore_request_duration_seconds_bucket{op=\"recipes\",le=\"0.5\"} 1"
        ));
    }
}
