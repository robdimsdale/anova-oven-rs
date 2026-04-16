use defmt::{debug, error, info, warn};
use embassy_time::{with_timeout, Duration, Instant};
use heapless::Vec as HeaplessVec;

use crate::api::{fetch_current_cook, fetch_recipes, fetch_status, send_start, send_stop};
use crate::backlight::BacklightController;
use crate::events::InputEvent;
use crate::lcd::LcdController;

pub enum UIState {
    ShowIdle,
    ShowCook,
    BrowseRecipes { index: usize },
    ConfirmStopCooking,
}

const MENU_INACTIVITY_TIMEOUT_SECS: u64 = 15;
const STOP_CONFIRM_TIMEOUT_SECS: u64 = 5;
const LED_DIM_TIMER_SECS: u64 = 5;

const SERVER_RETRY_LOG_INTERVAL: u64 = 5;
const API_CALL_TIMEOUT_SECS: u64 = 3;
const POST_ACTION_COOK_REFRESH_DELAY_SECS: u64 = 1;

const COOK_POLL_INTERVAL: u64 = 10;
const RECIPE_POLL_INTERVAL: u64 = 3600;

// Polling backoff. Without this, repeated failed connections pile up sockets
// in smoltcp and packets in cyw43's tiny rx channel (4 buffers, hardcoded in
// cyw43 0.7.0). Slowing the poll cadence as failures accumulate gives those
// queues time to drain, which is what actually unsticks things — the cyw43
// driver itself can't be safely re-initialized at runtime.
const NORMAL_POLL_INTERVAL_SECS: u64 = 1;
const POLL_BACKOFF_TIER1_FAILS: u64 = 5;
const POLL_BACKOFF_TIER2_FAILS: u64 = 10;
const POLL_BACKOFF_TIER3_FAILS: u64 = 15;
const POLL_BACKOFF_TIER1_SECS: u64 = 5;
const POLL_BACKOFF_TIER2_SECS: u64 = 15;
const POLL_BACKOFF_TIER3_SECS: u64 = 30;

const IDLE: &str = "idle";
const COOK: &str = "cook";
const PENDING: &str = "pending";

const EVENT_QUEUE_CAPACITY: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum EventKind {
    PollStatus,
    PollCurrentCook,
    PollRecipes,
    APIStart,
    APIStop,
    InactivityCheck,
}

#[derive(Clone, Copy)]
struct ScheduledEvent {
    kind: EventKind,
    execution_time: Instant,
    priority: u8,
}

impl EventKind {
    fn priority(self) -> u8 {
        match self {
            EventKind::APIStart | EventKind::APIStop => 1,
            _ => 0,
        }
    }
}

#[derive(Clone, Copy)]
enum EnqueueMode {
    /// Replace existing entry of the same kind only if the new time is sooner.
    PreferEarlier,
    /// Always replace existing entry of the same kind with the new time.
    Replace,
}

struct EventQueue {
    events: HeaplessVec<ScheduledEvent, EVENT_QUEUE_CAPACITY>,
}

impl EventQueue {
    fn new() -> Self {
        Self {
            events: HeaplessVec::new(),
        }
    }

    fn enqueue(&mut self, kind: EventKind, execution_time: Instant, mode: EnqueueMode) {
        if let Some(existing) = self.events.iter_mut().find(|e| e.kind == kind) {
            existing.execution_time = match mode {
                EnqueueMode::PreferEarlier => existing.execution_time.min(execution_time),
                EnqueueMode::Replace => execution_time,
            };
            return;
        }

        let event = ScheduledEvent {
            kind,
            execution_time,
            priority: kind.priority(),
        };

        if self.events.push(event).is_err() {
            error!(
                "Event queue overflow (capacity {}); dropping event",
                EVENT_QUEUE_CAPACITY
            );
        }
    }

    fn contains(&self, kind: EventKind) -> bool {
        self.events.iter().any(|e| e.kind == kind)
    }

    fn remove(&mut self, kind: EventKind) {
        if let Some(idx) = self.events.iter().position(|e| e.kind == kind) {
            self.events.swap_remove(idx);
        }
    }

    /// Returns the index of the soonest-due event, breaking ties by higher priority.
    fn soonest_index(&self) -> Option<usize> {
        self.events
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                a.execution_time
                    .cmp(&b.execution_time)
                    .then(b.priority.cmp(&a.priority))
            })
            .map(|(i, _)| i)
    }

    fn next_due_at(&self) -> Option<Instant> {
        self.soonest_index().map(|i| self.events[i].execution_time)
    }

    fn pop_due(&mut self, now: Instant) -> Option<ScheduledEvent> {
        let idx = self.soonest_index()?;
        if self.events[idx].execution_time > now {
            return None;
        }
        Some(self.events.swap_remove(idx))
    }
}

pub(crate) struct AppState {
    tick: u64,
    ui_state: UIState,
    baseline_reentered_at: Option<Instant>,
    current_cook: Option<anova_oven_api::CurrentCook>,
    latest_status: Option<anova_oven_api::OvenStatus>,
    recipes: alloc::vec::Vec<anova_oven_api::Recipe>,
    server_offline: bool,
    server_fail_count: u64,
    pending_start_recipe_id: Option<alloc::string::String>,
    event_queue: EventQueue,

    backlight_controller: BacklightController,
    lcd_controller: LcdController,
}

impl AppState {
    pub(crate) fn new(
        backlight_controller: BacklightController,
        lcd_controller: LcdController,
    ) -> Self {
        let now = Instant::now();
        let mut event_queue = EventQueue::new();
        event_queue.enqueue(EventKind::PollStatus, now, EnqueueMode::PreferEarlier);
        event_queue.enqueue(EventKind::PollCurrentCook, now, EnqueueMode::PreferEarlier);
        event_queue.enqueue(EventKind::PollRecipes, now, EnqueueMode::PreferEarlier);

        Self {
            tick: 0,
            ui_state: UIState::ShowIdle,
            baseline_reentered_at: Some(now),
            current_cook: None,
            latest_status: None,
            recipes: alloc::vec::Vec::new(),
            server_offline: false,
            server_fail_count: 0,
            pending_start_recipe_id: None,
            event_queue,
            backlight_controller,
            lcd_controller,
        }
    }

    pub(crate) async fn render_wifi_init(&mut self) {
        self.lcd_controller.render_wifi_init().await;
    }

    pub(crate) async fn render_dhcp_init(&mut self) {
        self.lcd_controller.render_dhcp_init().await;
    }

    pub(crate) fn next_event_due_at(&self) -> Option<Instant> {
        self.event_queue.next_due_at()
    }

    pub(crate) async fn handle_due_scheduled_event(&mut self, stack: embassy_net::Stack<'static>) {
        let now = Instant::now();
        let Some(event) = self.event_queue.pop_due(now) else {
            return;
        };

        match event.kind {
            EventKind::PollStatus => self.handle_poll_status(stack).await,
            EventKind::PollCurrentCook => self.handle_poll_current_cook(stack).await,
            EventKind::PollRecipes => self.handle_poll_recipes(stack).await,
            EventKind::APIStart => self.handle_api_start(stack).await,
            EventKind::APIStop => self.handle_api_stop(stack).await,
            EventKind::InactivityCheck => self.handle_inactivity_check(),
        }
    }

    fn next_poll_interval_secs(&self) -> u64 {
        match self.server_fail_count {
            n if n >= POLL_BACKOFF_TIER3_FAILS => POLL_BACKOFF_TIER3_SECS,
            n if n >= POLL_BACKOFF_TIER2_FAILS => POLL_BACKOFF_TIER2_SECS,
            n if n >= POLL_BACKOFF_TIER1_FAILS => POLL_BACKOFF_TIER1_SECS,
            _ => NORMAL_POLL_INTERVAL_SECS,
        }
    }

    fn poll_action_in_flight(&self) -> bool {
        self.event_queue.contains(EventKind::APIStart)
            || self.event_queue.contains(EventKind::APIStop)
    }

    async fn handle_api_start(&mut self, stack: embassy_net::Stack<'static>) {
        let Some(recipe_id) = self.pending_start_recipe_id.take() else {
            warn!("APIStart event fired but no recipe id was staged");
            return;
        };
        info!("Sending POST /start with recipe id: {}", recipe_id.as_str());
        if with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            send_start(stack, recipe_id.as_str()),
        )
        .await
        .is_err()
        {
            warn!("POST /start: timed out");
        }
    }

    async fn handle_api_stop(&mut self, stack: embassy_net::Stack<'static>) {
        if with_timeout(Duration::from_secs(API_CALL_TIMEOUT_SECS), send_stop(stack))
            .await
            .is_err()
        {
            warn!("POST /stop: timed out");
        }
    }

    async fn handle_poll_status(&mut self, stack: embassy_net::Stack<'static>) {
        if !self.poll_action_in_flight() {
            self.update_status(stack).await;
        }
        self.tick += 1;

        let interval = self
            .next_poll_interval_secs()
            .max(NORMAL_POLL_INTERVAL_SECS);
        self.event_queue.enqueue(
            EventKind::PollStatus,
            Instant::now() + Duration::from_secs(interval),
            EnqueueMode::PreferEarlier,
        );
    }

    async fn handle_poll_current_cook(&mut self, stack: embassy_net::Stack<'static>) {
        if !self.poll_action_in_flight() {
            self.update_current_cook(stack).await;
            self.reconcile_current_cook_recipe_title();
            self.sync_status_ui_state();
        }

        let interval = self.next_poll_interval_secs().max(COOK_POLL_INTERVAL);
        self.event_queue.enqueue(
            EventKind::PollCurrentCook,
            Instant::now() + Duration::from_secs(interval),
            EnqueueMode::PreferEarlier,
        );
    }

    async fn handle_poll_recipes(&mut self, stack: embassy_net::Stack<'static>) {
        if !self.poll_action_in_flight() {
            self.update_recipes(stack).await;
            self.reconcile_current_cook_recipe_title();
        }

        let interval = self.next_poll_interval_secs().max(RECIPE_POLL_INTERVAL);
        self.event_queue.enqueue(
            EventKind::PollRecipes,
            Instant::now() + Duration::from_secs(interval),
            EnqueueMode::PreferEarlier,
        );
    }

    pub(crate) async fn handle_user_event(&mut self, event: InputEvent) {
        let now = Instant::now();

        debug!("Setting backlight to full: (input activity)");
        self.backlight_controller.set_full();

        match self.ui_state {
            UIState::ShowCook => match event {
                InputEvent::EncoderCCW => {
                    info!("Entering stop-confirm mode: encoder CCW");
                    self.ui_state = UIState::ConfirmStopCooking;
                }
                InputEvent::EncoderCW => {
                    // Explicit no-op while cooking to keep this branch ready for future behavior.
                    debug!("Ignoring encoder CW while active cook status is shown");
                }
                InputEvent::EncoderButton => {
                    // No-op while showing cook status.
                }
            },
            UIState::ShowIdle => match event {
                InputEvent::EncoderCW => {
                    if !self.recipes.is_empty() {
                        self.ui_state = UIState::BrowseRecipes { index: 0 };
                    }
                }
                InputEvent::EncoderCCW => {
                    // No-op while at the top-level idle status view.
                }
                InputEvent::EncoderButton => {
                    // No-op while showing idle status.
                }
            },
            UIState::BrowseRecipes { ref mut index } => match event {
                InputEvent::EncoderCW => {
                    if self.recipes.is_empty() {
                        return;
                    }
                    *index = (*index + 1).min(self.recipes.len() - 1);
                }
                InputEvent::EncoderCCW => {
                    if self.recipes.is_empty() {
                        return;
                    }

                    if *index == 0 {
                        info!("Exiting recipe browser: encoder CCW past index 0");
                        self.enter_baseline_state();
                    } else {
                        *index -= 1;
                    }
                }
                InputEvent::EncoderButton => {
                    if let Some(recipe) = self.recipes.get(*index).cloned() {
                        self.apply_optimistic_start_state(&recipe);
                        self.queue_start_action(recipe.id.clone(), now);
                    }
                }
            },
            UIState::ConfirmStopCooking => match event {
                InputEvent::EncoderCCW => {
                    // Stay in confirm; the inactivity reschedule below pushes the deadline out.
                }
                InputEvent::EncoderCW => {
                    info!("Exiting stop-confirm mode: encoder CW");
                    self.enter_baseline_state();
                }
                InputEvent::EncoderButton => {
                    info!("Sending POST /stop (confirm-stop mode)");
                    self.apply_optimistic_stop_state();
                    self.queue_stop_action(now);
                    self.enter_baseline_state();
                }
            },
        }

        self.update_baseline_timer_for_current_view(now);
        self.reschedule_inactivity_check(now);
    }

    fn reschedule_inactivity_check(&mut self, now: Instant) {
        let timeout_secs = match self.ui_state {
            UIState::ShowIdle | UIState::ShowCook => {
                self.event_queue.remove(EventKind::InactivityCheck);
                return;
            }
            UIState::ConfirmStopCooking => STOP_CONFIRM_TIMEOUT_SECS,
            UIState::BrowseRecipes { .. } => MENU_INACTIVITY_TIMEOUT_SECS,
        };

        self.event_queue.enqueue(
            EventKind::InactivityCheck,
            now + Duration::from_secs(timeout_secs),
            EnqueueMode::Replace,
        );
    }

    fn handle_inactivity_check(&mut self) {
        if !self.is_status_view() {
            info!("Inactivity timeout: reverting to status view");
            self.enter_baseline_state();
        }
    }

    pub(crate) async fn render_current_view(&mut self) {
        self.apply_backlight_policy();

        if self.server_offline {
            self.lcd_controller.render_server_offline(self.tick).await;
            return;
        }

        match &self.ui_state {
            UIState::ShowIdle | UIState::ShowCook => {
                self.lcd_controller
                    .render_status_display(
                        self.tick,
                        self.latest_status.as_ref(),
                        self.current_cook.as_ref(),
                    )
                    .await;
            }
            UIState::BrowseRecipes { index } => {
                self.lcd_controller
                    .render_recipe_browser(&self.recipes, *index, self.tick)
                    .await;
            }
            UIState::ConfirmStopCooking => {
                self.lcd_controller
                    .render_stop_confirmation(
                        self.tick,
                        self.latest_status.as_ref(),
                        self.current_cook.as_ref(),
                    )
                    .await;
            }
        }
    }

    async fn update_status(&mut self, stack: embassy_net::Stack<'static>) {
        self.latest_status = match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_status(stack),
        )
        .await
        {
            Ok(Some(status)) => {
                self.set_server_offline(false);
                Some(status)
            }
            Ok(None) => {
                warn!("GET /status: failed");
                self.set_server_offline(true);
                None
            }
            Err(_) => {
                warn!("GET /status: timed out");
                self.set_server_offline(true);
                None
            }
        };
    }

    async fn update_current_cook(&mut self, stack: embassy_net::Stack<'static>) {
        self.current_cook = match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_current_cook(stack),
        )
        .await
        {
            Ok(maybe_cook) => {
                self.set_server_offline(false);
                maybe_cook
            }
            Err(_) => {
                warn!("GET /current-cook: timed out");
                self.set_server_offline(true);
                None
            }
        }
    }

    async fn update_recipes(&mut self, stack: embassy_net::Stack<'static>) {
        self.recipes = match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_recipes(stack),
        )
        .await
        {
            Ok(recipes) => {
                self.set_server_offline(false);
                recipes
            }
            Err(_) => {
                warn!("GET /recipes: timed out");
                self.set_server_offline(true);
                alloc::vec::Vec::new()
            }
        };
    }

    fn enter_baseline_state(&mut self) {
        self.ui_state = UIState::ShowIdle;
        self.sync_status_ui_state();
        self.update_baseline_timer_for_current_view(Instant::now());
        self.event_queue.remove(EventKind::InactivityCheck);
    }

    fn set_server_offline(&mut self, offline: bool) {
        if offline {
            if !self.server_offline {
                warn!("Lost communication with server");
            } else {
                if self.server_fail_count % SERVER_RETRY_LOG_INTERVAL == 0 {
                    warn!(
                        "Still offline: {} consecutive server failures",
                        self.server_fail_count
                    );
                }
            }
            self.server_offline = true;
            self.latest_status = None;
            self.current_cook = None;
            self.server_fail_count = self.server_fail_count.saturating_add(1);
        } else {
            if self.server_offline {
                info!("Server communication restored");
            }
            self.server_offline = false;
            self.server_fail_count = 0;
        }

        self.sync_status_ui_state();
    }

    fn apply_backlight_policy(&mut self) {
        if self.server_offline {
            debug!("Setting backlight to full: (server offline)");
            self.backlight_controller.set_full();
            return;
        }

        let should_dim = self.is_status_view()
            && !self.is_active_cook()
            && self
                .baseline_reentered_at
                .map(|t| t.elapsed().as_secs())
                .is_some_and(|elapsed| elapsed >= LED_DIM_TIMER_SECS);

        let should_full = !self.is_status_view() || self.is_active_cook();

        if should_dim {
            debug!("Setting backlight to dim: (idle baseline)");
            self.backlight_controller.set_dim();
        } else if should_full {
            debug!("Setting backlight to full: (active state)");
            self.backlight_controller.set_full();
        }
    }

    fn is_active_cook(&self) -> bool {
        self.current_cook.is_some()
            || self
                .latest_status
                .as_ref()
                .map(|status| status.mode.as_str())
                .is_some_and(|mode| mode != "idle")
    }

    fn update_baseline_timer_for_current_view(&mut self, now: Instant) {
        self.baseline_reentered_at = if self.is_status_view() {
            Some(now)
        } else {
            None
        };
    }

    fn is_status_view(&self) -> bool {
        matches!(self.ui_state, UIState::ShowIdle | UIState::ShowCook)
    }

    fn reconcile_current_cook_recipe_title(&mut self) {
        let Some(cook) = self.current_cook.as_mut() else {
            return;
        };

        let Some(recipe_id) = cook.recipe_id.as_deref() else {
            return;
        };

        if let Some(recipe) = self.recipes.iter().find(|recipe| recipe.id == recipe_id) {
            cook.recipe_title = recipe.title.clone();
        }
    }

    fn apply_optimistic_stop_state(&mut self) {
        self.current_cook = None;

        if let Some(status) = self.latest_status.as_mut() {
            status.mode = IDLE.into();
            status.timer_mode = IDLE.into();
            status.timer_current_secs = 0;
            status.target_temperature_c = None;
            status.steam_target_pct = None;
        }

        self.sync_status_ui_state();
    }

    fn apply_optimistic_start_state(&mut self, recipe: &anova_oven_api::Recipe) {
        let cook_stage_count = recipe
            .stages
            .iter()
            .filter(|stage| stage.kind.as_str() == COOK)
            .count();

        self.current_cook = Some(anova_oven_api::CurrentCook {
            recipe_title: recipe.title.clone(),
            recipe_id: Some(recipe.id.clone()),
            started_at: PENDING.into(),
            stages: recipe.stages.clone(),
            cook_stage_count,
            total_stage_count: recipe.stages.len(),
        });

        self.ui_state = UIState::ShowCook;
    }

    fn queue_stop_action(&mut self, now: Instant) {
        self.event_queue
            .enqueue(EventKind::APIStop, now, EnqueueMode::PreferEarlier);
        self.queue_post_action_refresh(now);
    }

    fn queue_start_action(&mut self, recipe_id: alloc::string::String, now: Instant) {
        self.pending_start_recipe_id = Some(recipe_id);
        self.event_queue
            .enqueue(EventKind::APIStart, now, EnqueueMode::PreferEarlier);
        self.queue_post_action_refresh(now);
    }

    fn queue_post_action_refresh(&mut self, now: Instant) {
        let refresh_at = now + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS);
        self.event_queue.enqueue(
            EventKind::PollStatus,
            refresh_at,
            EnqueueMode::PreferEarlier,
        );
        self.event_queue.enqueue(
            EventKind::PollCurrentCook,
            refresh_at,
            EnqueueMode::PreferEarlier,
        );
    }

    fn sync_status_ui_state(&mut self) {
        if !matches!(self.ui_state, UIState::ShowIdle | UIState::ShowCook) {
            return;
        }

        self.ui_state = if self.is_active_cook() {
            UIState::ShowCook
        } else {
            UIState::ShowIdle
        };
    }
}
