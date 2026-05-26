//! Pure data types and decision functions for the user-facing state machine.
//!
//! `ApiSnapshot` is the input contract: a plain-data view of what we last
//! heard from the server. `AppState` is the FSM state itself. `ViewSpec` is
//! what each state asks the display to render. The actual `execute_*`
//! handlers (which `.await` input/timer events and call into `Display` /
//! `ApiClient` / `BacklightController`) stay in the bin — they are where
//! effects live; the decisions live here.

use alloc::string::String;
use alloc::vec::Vec;

use embassy_time::{Duration, Instant};
use portable_atomic_util::Arc;

/// `mode` / `timer_mode` value that indicates the oven is idle. The
/// optimistic-idle view overwrites these fields with this string so the UI
/// updates immediately on Stop without waiting for the server round-trip
/// (review §3.1 flags this as a stringly-typed contract worth tightening).
pub const IDLE: &str = "idle";

/// Number of consecutive fast-poll failures before we consider the server
/// offline. Lives here so `ApiSnapshot::is_offline` is self-contained.
pub const OFFLINE_THRESHOLD: u64 = 3;

/// Latest data we've fetched from the server, plus poll health. Cloned on
/// every render — `recipes` is `Arc` so that clone is cheap; `status` /
/// `current_cook` are still deep-cloned today (review §2.1 #3 suggests
/// `Arc`-wrapping these too).
#[derive(Clone)]
pub struct ApiSnapshot {
    pub status: Option<anova_oven_api::OvenStatus>,
    pub current_cook: Option<anova_oven_api::CurrentCook>,
    pub recipes: Arc<Vec<anova_oven_api::Recipe>>,
    pub fail_count: u64,
    pub last_success_at: Option<Instant>,
}

impl Default for ApiSnapshot {
    fn default() -> Self {
        Self {
            status: None,
            current_cook: None,
            recipes: Arc::new(Vec::new()),
            fail_count: 0,
            last_success_at: None,
        }
    }
}

impl ApiSnapshot {
    pub fn is_offline(&self) -> bool {
        self.fail_count >= OFFLINE_THRESHOLD
    }

    pub fn has_first_data(&self) -> bool {
        self.last_success_at.is_some()
    }

    /// True if we believe a cook is in progress. Either:
    /// - we have a `current_cook` payload, or
    /// - the status `mode` is anything other than `"idle"` (server is
    ///   actively heating but the cook record hasn't arrived yet).
    pub fn is_cooking(&self) -> bool {
        self.current_cook.is_some()
            || self
                .status
                .as_ref()
                .is_some_and(|status| status.mode.as_str() != IDLE)
    }
}

/// FSM state. Each variant maps to one `execute_*` handler in the bin's
/// `state.rs` that handles the input/timer events for that screen.
#[derive(Clone)]
pub enum AppState {
    Offline,
    Idle,
    Cooking {
        optimistic_recipe_title: Option<String>,
    },
    BrowseRecipes {
        index: usize,
    },
    StartPending {
        recipe_title: String,
        recipe_id: String,
        since: Instant,
    },
    ConfirmStop,
    StopPending {
        since: Instant,
    },
    AwaitNextStage {
        next_description: String,
    },
}

impl Default for AppState {
    fn default() -> Self {
        Self::Idle
    }
}

impl AppState {
    /// Stable numeric identifier persisted across resets via the persist
    /// region. Add new variants here when extending `AppState`; never
    /// renumber existing ones (review §6.5 suggests a typed
    /// `#[repr(u32)] enum AppStateId` to localize this contract).
    pub fn discriminant(&self) -> u32 {
        match self {
            AppState::Offline => 1,
            AppState::Idle => 2,
            AppState::Cooking { .. } => 3,
            AppState::BrowseRecipes { .. } => 4,
            AppState::StartPending { .. } => 5,
            AppState::ConfirmStop => 6,
            AppState::StopPending { .. } => 7,
            AppState::AwaitNextStage { .. } => 8,
        }
    }
}

/// Human-readable label for an `AppState::discriminant()` value, or
/// `None` if `d` isn't a known AppState discriminant (e.g. it's an
/// `INIT_STAGE_*` sentinel — use [`crate::reset::init_stage_name`]
/// for those).
///
/// Co-located with `discriminant()` so adding a new variant means
/// editing one screen. Also parsed by the bin's `dump-persist`
/// debug-port tool so any over-debugger snapshot shows the same labels
/// as `/health` — keep arms as `N => Some("VariantName"),` one per
/// line so the regex parser keeps working.
///
/// `name_for_discriminant` rather than an impl method because the
/// caller has only a `u32` (read from the persist region), not an
/// `AppState`.
pub fn app_state_name(d: u32) -> Option<&'static str> {
    match d {
        0 => Some("(unset / pre-init)"),
        1 => Some("Offline"),
        2 => Some("Idle"),
        3 => Some("Cooking"),
        4 => Some("BrowseRecipes"),
        5 => Some("StartPending"),
        6 => Some("ConfirmStop"),
        7 => Some("StopPending"),
        8 => Some("AwaitNextStage"),
        _ => None,
    }
}

impl AppState {
    pub fn backlight_policy(&self) -> BacklightPolicy {
        match self {
            AppState::Idle | AppState::Cooking { .. } => {
                BacklightPolicy::FullThenDimAfter(Duration::from_secs(5))
            }
            AppState::Offline
            | AppState::BrowseRecipes { .. }
            | AppState::StartPending { .. }
            | AppState::ConfirmStop
            | AppState::StopPending { .. }
            | AppState::AwaitNextStage { .. } => BacklightPolicy::Full,
        }
    }

    pub fn idle_dim_delay(&self) -> Duration {
        match self.backlight_policy() {
            BacklightPolicy::FullThenDimAfter(delay) => delay,
            BacklightPolicy::Full | BacklightPolicy::Dim => Duration::from_secs(5),
        }
    }
}

/// Backlight policy a state requests. The bin's `BacklightController`
/// translates this into PWM/GPIO calls.
pub enum BacklightPolicy {
    Full,
    Dim,
    FullThenDimAfter(Duration),
}

/// A screen the FSM wants the display task to render. Plain data — the
/// LCD driver (HD44780 byte-strobing) consumes it.
#[derive(Clone)]
pub enum ViewSpec {
    WifiInit,
    DhcpInit,
    Connecting,
    ServerOffline,
    Status {
        status: Option<anova_oven_api::OvenStatus>,
        cook: Option<anova_oven_api::CurrentCook>,
    },
    RecipeBrowser {
        count: usize,
        index: usize,
        title: String,
    },
    StopConfirmation {
        status: Option<anova_oven_api::OvenStatus>,
        cook: Option<anova_oven_api::CurrentCook>,
    },
    StartingCook {
        recipe_title: String,
    },
    NextStagePrompt {
        recipe_title: String,
    },
    Recovery {
        reset_count: u32,
        panic_count: u32,
        message: Option<String>,
    },
}

/// Decide which "base" state to enter when arriving from an unrelated state
/// (e.g. recovering from `Offline`, or after a successful Start that
/// transitioned through `StartPending`). Mirrors `is_cooking`: any sign of
/// an active cook → `Cooking`, otherwise `Idle`.
pub fn baseline_state_for(snap: &ApiSnapshot) -> AppState {
    if snap.is_cooking() {
        AppState::Cooking {
            optimistic_recipe_title: None,
        }
    } else {
        AppState::Idle
    }
}

/// What the idle screen should show. Until we have first data, render the
/// `Connecting` placeholder; afterwards show whatever the latest status /
/// (lack of) cook says.
pub fn idle_view(snap: &ApiSnapshot) -> ViewSpec {
    if !snap.has_first_data() {
        ViewSpec::Connecting
    } else {
        ViewSpec::Status {
            status: snap.status.clone(),
            cook: snap.current_cook.clone(),
        }
    }
}

/// What the cooking screen should show. Prefers the real `CurrentCook` if
/// the server has produced one; otherwise (the server is heating per
/// `status.is_cooking()` but the cook record hasn't arrived yet) synthesize
/// a placeholder from `optimistic_recipe_title` so the UI doesn't blink
/// back to idle after a Start.
pub fn cooking_view(snap: &ApiSnapshot, optimistic_recipe_title: Option<&str>) -> ViewSpec {
    let cook = if snap.current_cook.is_some() {
        snap.current_cook.clone()
    } else if snap
        .status
        .as_ref()
        .is_some_and(|status| status.is_cooking())
    {
        optimistic_recipe_title.map(|title| anova_oven_api::CurrentCook {
            recipe_title: title.into(),
            recipe_id: None,
            started_at: String::from("pending"),
            stages: alloc::vec::Vec::new(),
            cook_stage_count: 0,
            total_stage_count: 0,
        })
    } else {
        None
    };

    ViewSpec::Status {
        status: snap.status.clone(),
        cook,
    }
}

/// Build an "optimistic idle" view by overwriting the live status's
/// `mode`/`timer_mode` to `IDLE`, clearing the timer, and dropping the
/// target temperature. Used by `StopPending` so the UI flips to idle
/// immediately while we wait for the server to confirm the stop.
pub fn optimistic_idle_view(snap: &ApiSnapshot) -> ViewSpec {
    let status = snap.status.as_ref().map(|status| {
        let mut optimistic = status.clone();
        optimistic.mode = String::from(IDLE);
        optimistic.timer_mode = String::from(IDLE);
        optimistic.timer_current_secs = 0;
        optimistic.target_temperature_c = None;
        optimistic.steam_target_pct = None;
        optimistic
    });

    ViewSpec::Status { status, cook: None }
}

/// If the server has flagged that the next stage is ready (multi-stage
/// recipes), return the description to prompt the user with. Falls back
/// to "Next stage" if the server didn't supply a description.
pub fn next_stage_prompt(snap: &ApiSnapshot) -> Option<String> {
    let progress = snap.status.as_ref()?.cook_progress.as_ref()?;
    if !progress.next_stage_ready {
        return None;
    }
    Some(
        progress
            .next_stage_description
            .clone()
            .unwrap_or_else(|| String::from("Next stage")),
    )
}

/// Best title to display for the currently-running cook. Prefers the
/// real `current_cook.display_name()`, falls back to `status.cook_progress`'s
/// title if usable (non-empty and not the synthetic `"[manual]"`), and
/// finally to a generic "Active cook" label.
pub fn active_recipe_title(snap: &ApiSnapshot) -> String {
    if let Some(cook) = snap.current_cook.as_ref() {
        return String::from(cook.display_name());
    }
    if let Some(progress) = snap.status.as_ref().and_then(|s| s.cook_progress.as_ref()) {
        if !progress.recipe_title.is_empty() && progress.recipe_title != "[manual]" {
            return progress.recipe_title.clone();
        }
    }
    String::from("Active cook")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ApiSnapshot {
        ApiSnapshot::default()
    }

    /// Base OvenStatus JSON with every required field present. Tests
    /// construct an instance and then mutate fields directly rather than
    /// hand-spelling the full struct (which has ~30 fields).
    const STATUS_JSON: &str = r#"{
        "mode": "idle",
        "temperature_unit": "C",
        "temperature_c": 25.0,
        "temperature_bulbs_mode": "dry",
        "dry_top_temperature_c": 25.0,
        "dry_bottom_temperature_c": 25.0,
        "wet_bulb_temperature_c": 25.0,
        "timer_current_secs": 0,
        "timer_total_secs": 0,
        "timer_mode": "idle",
        "steam_pct": 0.0,
        "steam_generator_mode": "idle",
        "boiler_celsius": 25.0,
        "boiler_watts": 0.0,
        "boiler_descale_required": false,
        "evaporator_celsius": 25.0,
        "evaporator_watts": 0.0,
        "fan_speed": 0,
        "heating_element_top_on": false,
        "heating_element_top_watts": 0.0,
        "heating_element_rear_on": false,
        "heating_element_rear_watts": 0.0,
        "heating_element_bottom_on": false,
        "heating_element_bottom_watts": 0.0,
        "lamp_on": false,
        "lamp_preference": "off",
        "vent_open": false,
        "door_open": false,
        "water_tank_empty": false
    }"#;

    fn idle_status() -> anova_oven_api::OvenStatus {
        serde_json::from_str(STATUS_JSON).expect("idle fixture must parse")
    }

    fn cooking_status() -> anova_oven_api::OvenStatus {
        let mut s = idle_status();
        s.mode = String::from("preheat");
        s.timer_mode = String::from("running");
        s
    }

    fn cook_progress(
        next_stage_ready: bool,
        next_stage_description: Option<&str>,
        recipe_title: &str,
    ) -> anova_oven_api::CookProgress {
        let next_desc_json = match next_stage_description {
            Some(d) => alloc::format!(r#""{d}""#),
            None => String::from("null"),
        };
        let json = alloc::format!(
            r#"{{
                "recipe_title": "{recipe_title}",
                "current_stage_index": 0,
                "total_stage_count": 1,
                "current_stage_description": "stage 0",
                "current_stage_kind": "cook",
                "next_stage_ready": {next_stage_ready},
                "next_stage_description": {next_desc_json}
            }}"#
        );
        serde_json::from_str(&json).expect("cook_progress fixture must parse")
    }

    fn current_cook(recipe_title: &str) -> anova_oven_api::CurrentCook {
        let json = alloc::format!(
            r#"{{
                "recipe_title": "{recipe_title}",
                "started_at": "now",
                "stages": [],
                "cook_stage_count": 0,
                "total_stage_count": 0
            }}"#
        );
        serde_json::from_str(&json).expect("current_cook fixture must parse")
    }

    // --- ApiSnapshot::is_offline / has_first_data / is_cooking ---

    #[test]
    fn is_offline_at_threshold() {
        let mut snap = snapshot();
        snap.fail_count = OFFLINE_THRESHOLD - 1;
        assert!(!snap.is_offline());
        snap.fail_count = OFFLINE_THRESHOLD;
        assert!(snap.is_offline());
        snap.fail_count = OFFLINE_THRESHOLD + 100;
        assert!(snap.is_offline());
    }

    #[test]
    fn has_first_data_only_after_success() {
        let mut snap = snapshot();
        assert!(!snap.has_first_data());
        snap.last_success_at = Some(Instant::from_ticks(1));
        assert!(snap.has_first_data());
    }

    #[test]
    fn is_cooking_when_current_cook_is_present() {
        let mut snap = snapshot();
        // A non-empty CurrentCook means cooking, regardless of status.mode.
        snap.current_cook = Some(current_cook("x"));
        assert!(snap.is_cooking());
    }

    #[test]
    fn is_cooking_when_status_mode_is_not_idle() {
        let mut snap = snapshot();
        snap.status = Some(cooking_status());
        assert!(snap.is_cooking());
    }

    #[test]
    fn is_not_cooking_when_status_is_idle_and_no_cook() {
        let mut snap = snapshot();
        snap.status = Some(idle_status());
        assert!(!snap.is_cooking());
    }

    #[test]
    fn is_not_cooking_when_no_status_and_no_cook() {
        let snap = snapshot();
        assert!(!snap.is_cooking());
    }

    // --- AppState::discriminant — pin the persisted IDs ---

    #[test]
    fn discriminants_are_stable() {
        // These values are persisted across resets via `last_app_state`.
        // Renumbering would silently misclassify reset-during-state on
        // upgraded firmware (the persist MAGIC bump catches it on the
        // *next* device, not this one).
        assert_eq!(AppState::Offline.discriminant(), 1);
        assert_eq!(AppState::Idle.discriminant(), 2);
        assert_eq!(
            AppState::Cooking {
                optimistic_recipe_title: None
            }
            .discriminant(),
            3
        );
        assert_eq!(AppState::BrowseRecipes { index: 0 }.discriminant(), 4);
        assert_eq!(
            AppState::StartPending {
                recipe_title: String::new(),
                recipe_id: String::new(),
                since: Instant::from_ticks(0),
            }
            .discriminant(),
            5
        );
        assert_eq!(AppState::ConfirmStop.discriminant(), 6);
        assert_eq!(
            AppState::StopPending {
                since: Instant::from_ticks(0)
            }
            .discriminant(),
            7
        );
        assert_eq!(
            AppState::AwaitNextStage {
                next_description: String::new()
            }
            .discriminant(),
            8
        );
    }

    #[test]
    fn every_app_state_variant_has_a_name() {
        // The dual to `discriminants_are_stable`: every variant's
        // discriminant must round-trip to a `Some(_)` name. Adding a
        // new variant + bumping its `discriminant()` arm without also
        // updating `app_state_name` would otherwise show up as
        // "Unknown" on `/health` and over `dump-persist.sh` — silently
        // wrong precisely when debugging.
        let all: [AppState; 8] = [
            AppState::Offline,
            AppState::Idle,
            AppState::Cooking {
                optimistic_recipe_title: None,
            },
            AppState::BrowseRecipes { index: 0 },
            AppState::StartPending {
                recipe_title: String::new(),
                recipe_id: String::new(),
                since: Instant::from_ticks(0),
            },
            AppState::ConfirmStop,
            AppState::StopPending {
                since: Instant::from_ticks(0),
            },
            AppState::AwaitNextStage {
                next_description: String::new(),
            },
        ];
        for s in all {
            let d = s.discriminant();
            assert!(
                app_state_name(d).is_some(),
                "AppState (discriminant {d}) has no app_state_name mapping",
            );
        }
        // Unknown/out-of-range discriminants return None so callers can
        // distinguish "we don't know" from a valid label.
        assert_eq!(app_state_name(9), None);
        assert_eq!(app_state_name(99), None);
        // 100/101 are INIT_STAGE_* sentinels, owned by reset.rs.
        assert_eq!(app_state_name(100), None);
        assert_eq!(app_state_name(101), None);
    }

    // --- AppState::backlight_policy ---

    #[test]
    fn idle_and_cooking_dim_after_5s() {
        match AppState::Idle.backlight_policy() {
            BacklightPolicy::FullThenDimAfter(d) => assert_eq!(d, Duration::from_secs(5)),
            _ => panic!("Idle should be FullThenDimAfter(5s)"),
        }
        let cooking = AppState::Cooking {
            optimistic_recipe_title: None,
        };
        match cooking.backlight_policy() {
            BacklightPolicy::FullThenDimAfter(d) => assert_eq!(d, Duration::from_secs(5)),
            _ => panic!("Cooking should be FullThenDimAfter(5s)"),
        }
    }

    #[test]
    fn interactive_states_stay_full_brightness() {
        for s in [
            AppState::Offline,
            AppState::BrowseRecipes { index: 0 },
            AppState::ConfirmStop,
            AppState::StopPending {
                since: Instant::from_ticks(0),
            },
            AppState::AwaitNextStage {
                next_description: String::new(),
            },
        ] {
            assert!(matches!(s.backlight_policy(), BacklightPolicy::Full));
        }
    }

    // --- baseline_state_for ---

    #[test]
    fn baseline_is_idle_when_not_cooking() {
        let snap = snapshot();
        assert!(matches!(baseline_state_for(&snap), AppState::Idle));
    }

    #[test]
    fn baseline_is_cooking_when_status_indicates_cook() {
        let mut snap = snapshot();
        snap.status = Some(cooking_status());
        assert!(matches!(
            baseline_state_for(&snap),
            AppState::Cooking {
                optimistic_recipe_title: None
            }
        ));
    }

    // --- idle_view ---

    #[test]
    fn idle_view_shows_connecting_before_first_data() {
        let snap = snapshot();
        assert!(matches!(idle_view(&snap), ViewSpec::Connecting));
    }

    #[test]
    fn idle_view_shows_status_after_first_data() {
        let mut snap = snapshot();
        snap.last_success_at = Some(Instant::from_ticks(1));
        snap.status = Some(idle_status());
        assert!(matches!(idle_view(&snap), ViewSpec::Status { .. }));
    }

    // --- cooking_view ---

    #[test]
    fn cooking_view_uses_real_cook_when_present() {
        let mut snap = snapshot();
        snap.current_cook = Some(current_cook("Real Cook"));
        match cooking_view(&snap, Some("Optimistic")) {
            ViewSpec::Status { cook: Some(c), .. } => {
                assert_eq!(c.recipe_title.as_str(), "Real Cook");
            }
            _ => panic!("expected ViewSpec::Status with a cook"),
        }
    }

    #[test]
    fn cooking_view_synthesizes_optimistic_when_status_is_cooking_but_no_cook() {
        let mut snap = snapshot();
        snap.status = Some(cooking_status());
        match cooking_view(&snap, Some("My Recipe")) {
            ViewSpec::Status { cook: Some(c), .. } => {
                assert_eq!(c.recipe_title.as_str(), "My Recipe");
                assert_eq!(c.recipe_id, None);
                assert_eq!(c.started_at.as_str(), "pending");
            }
            _ => panic!("expected optimistic placeholder cook"),
        }
    }

    #[test]
    fn cooking_view_has_no_cook_when_status_is_idle_and_no_current_cook() {
        let mut snap = snapshot();
        snap.status = Some(idle_status());
        match cooking_view(&snap, Some("My Recipe")) {
            ViewSpec::Status { cook, .. } => assert!(cook.is_none()),
            _ => panic!("expected ViewSpec::Status"),
        }
    }

    // --- optimistic_idle_view ---

    #[test]
    fn optimistic_idle_overwrites_mode_and_clears_timer() {
        let mut snap = snapshot();
        let mut status = cooking_status();
        status.timer_current_secs = 42;
        status.target_temperature_c = Some(180.0);
        status.steam_target_pct = Some(50.0);
        snap.status = Some(status);

        match optimistic_idle_view(&snap) {
            ViewSpec::Status {
                status: Some(s),
                cook,
            } => {
                assert_eq!(s.mode.as_str(), IDLE);
                assert_eq!(s.timer_mode.as_str(), IDLE);
                assert_eq!(s.timer_current_secs, 0);
                assert_eq!(s.target_temperature_c, None);
                assert_eq!(s.steam_target_pct, None);
                assert!(cook.is_none(), "optimistic idle clears the cook");
            }
            _ => panic!("expected ViewSpec::Status"),
        }
    }

    #[test]
    fn optimistic_idle_with_no_status_yields_none() {
        let snap = snapshot();
        match optimistic_idle_view(&snap) {
            ViewSpec::Status { status, cook } => {
                assert!(status.is_none());
                assert!(cook.is_none());
            }
            _ => panic!("expected ViewSpec::Status"),
        }
    }

    // --- next_stage_prompt ---

    #[test]
    fn next_stage_prompt_none_when_no_status() {
        let snap = snapshot();
        assert!(next_stage_prompt(&snap).is_none());
    }

    #[test]
    fn next_stage_prompt_none_when_no_cook_progress() {
        let mut snap = snapshot();
        snap.status = Some(idle_status());
        assert!(next_stage_prompt(&snap).is_none());
    }

    #[test]
    fn next_stage_prompt_none_when_next_stage_not_ready() {
        let mut snap = snapshot();
        let mut status = idle_status();
        status.cook_progress = Some(cook_progress(false, Some("more"), "r"));
        snap.status = Some(status);
        assert!(next_stage_prompt(&snap).is_none());
    }

    #[test]
    fn next_stage_prompt_returns_description_when_ready() {
        let mut snap = snapshot();
        let mut status = idle_status();
        status.cook_progress = Some(cook_progress(true, Some("Add salt"), "r"));
        snap.status = Some(status);
        assert_eq!(next_stage_prompt(&snap).as_deref(), Some("Add salt"));
    }

    #[test]
    fn next_stage_prompt_falls_back_to_default_label() {
        let mut snap = snapshot();
        let mut status = idle_status();
        status.cook_progress = Some(cook_progress(true, None, "r"));
        snap.status = Some(status);
        assert_eq!(next_stage_prompt(&snap).as_deref(), Some("Next stage"));
    }

    // --- active_recipe_title ---

    #[test]
    fn active_recipe_title_prefers_current_cook() {
        let mut snap = snapshot();
        snap.current_cook = Some(current_cook("From Cook"));
        assert_eq!(active_recipe_title(&snap), "From Cook");
    }

    #[test]
    fn active_recipe_title_falls_back_to_status_progress() {
        let mut snap = snapshot();
        let mut status = idle_status();
        status.cook_progress = Some(cook_progress(false, None, "From Progress"));
        snap.status = Some(status);
        assert_eq!(active_recipe_title(&snap), "From Progress");
    }

    #[test]
    fn active_recipe_title_skips_manual_sentinel() {
        let mut snap = snapshot();
        let mut status = idle_status();
        status.cook_progress = Some(cook_progress(false, None, "[manual]"));
        snap.status = Some(status);
        assert_eq!(active_recipe_title(&snap), "Active cook");
    }

    #[test]
    fn active_recipe_title_skips_empty_progress_title() {
        let mut snap = snapshot();
        let mut status = idle_status();
        status.cook_progress = Some(cook_progress(false, None, ""));
        snap.status = Some(status);
        assert_eq!(active_recipe_title(&snap), "Active cook");
    }

    #[test]
    fn active_recipe_title_default_when_no_data() {
        let snap = snapshot();
        assert_eq!(active_recipe_title(&snap), "Active cook");
    }
}
