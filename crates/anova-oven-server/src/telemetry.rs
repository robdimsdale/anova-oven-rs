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

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anova_oven_api::OvenStatus;
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, Unit};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tokio::sync::watch;

use crate::liveness::Liveness;

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

    gauge!(OVEN_COOK_ACTIVE).set(bool_gauge(status.mode != "idle"));
    gauge!(OVEN_DOOR_OPEN).set(bool_gauge(status.door_open));
    gauge!(OVEN_WATER_TANK_EMPTY).set(bool_gauge(status.water_tank_empty));
}

/// Mirror the read model's latest `OvenStatus` into the oven gauges for the
/// life of the process. Gauges keep their last value while upstream is down;
/// staleness is what `anova_upstream_last_state_timestamp_seconds` is for.
pub async fn run_oven_status_gauges(mut status_rx: watch::Receiver<Option<OvenStatus>>) {
    loop {
        if let Some(status) = status_rx.borrow_and_update().as_ref() {
            record_oven_status(status);
        }
        if status_rx.changed().await.is_err() {
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
            "temperature_bulbs_mode": "dry",
            "dry_top_temperature_c": 181.0,
            "dry_bottom_temperature_c": 179.0,
            "wet_bulb_temperature_c": 60.0,
            "timer_current_secs": 0,
            "timer_total_secs": 0,
            "timer_mode": "idle",
            "steam_pct": 0.0,
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
