use defmt::{debug, info, warn};
use embassy_time::{with_timeout, Duration, Instant};

use crate::api::{fetch_current_cook, fetch_recipes, fetch_status, send_start, send_stop};
use crate::backlight::BacklightController;
use crate::events::InputEvent;
use crate::lcd::LcdController;

pub enum UIState {
    ShowIdle,
    ShowCook,
    BrowseRecipes { index: usize },
    ConfirmStopCooking { last_input_at: Instant },
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

enum PendingApiAction {
    Stop,
    Start { recipe_id: alloc::string::String },
}

pub(crate) struct AppState {
    pub(crate) tick: u64,
    pub(crate) ui_state: UIState,
    pub(crate) last_input_at: Option<Instant>,
    pub(crate) baseline_reentered_at: Option<Instant>,
    pub(crate) current_cook: Option<anova_oven_api::CurrentCook>,
    pub(crate) latest_status: Option<anova_oven_api::OvenStatus>,
    pub(crate) recipes: alloc::vec::Vec<anova_oven_api::Recipe>,
    pub(crate) server_offline: bool,
    pub(crate) server_fail_count: u64,
    pending_api_action: Option<PendingApiAction>,
    queued_refresh_at: Option<Instant>,

    backlight_controller: BacklightController,
    lcd_controller: LcdController,
}

impl AppState {
    pub(crate) fn new(
        backlight_controller: BacklightController,
        lcd_controller: LcdController,
    ) -> Self {
        Self {
            tick: 0,
            ui_state: UIState::ShowIdle,
            last_input_at: None,
            baseline_reentered_at: Some(Instant::now()),
            current_cook: None,
            latest_status: None,
            recipes: alloc::vec::Vec::new(),
            server_offline: false,
            server_fail_count: 0,
            pending_api_action: None,
            queued_refresh_at: None,
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

    pub(crate) async fn init_data(&mut self, stack: embassy_net::Stack<'static>) {
        self.update_status(stack).await;
        self.update_current_cook(stack).await;
        self.update_recipes(stack).await;

        self.reconcile_current_cook_recipe_title();
        self.sync_status_ui_state();
    }

    pub(crate) fn next_poll_interval_secs(&self) -> u64 {
        match self.server_fail_count {
            n if n >= POLL_BACKOFF_TIER3_FAILS => POLL_BACKOFF_TIER3_SECS,
            n if n >= POLL_BACKOFF_TIER2_FAILS => POLL_BACKOFF_TIER2_SECS,
            n if n >= POLL_BACKOFF_TIER1_FAILS => POLL_BACKOFF_TIER1_SECS,
            _ => NORMAL_POLL_INTERVAL_SECS,
        }
    }

    pub(crate) fn update_inactivity_timeout(&mut self) {
        if let UIState::ConfirmStopCooking { last_input_at } = self.ui_state {
            if last_input_at.elapsed() >= Duration::from_secs(STOP_CONFIRM_TIMEOUT_SECS) {
                info!("Stop-confirm timeout: reverting to status view");
                self.enter_baseline_state();
            }
            return;
        }

        if let Some(t) = self.last_input_at {
            if t.elapsed() >= Duration::from_secs(MENU_INACTIVITY_TIMEOUT_SECS) {
                if !matches!(self.ui_state, UIState::ShowIdle | UIState::ShowCook) {
                    info!("Inactivity timeout: reverting to status view");
                    self.enter_baseline_state();
                }
                self.last_input_at = None;
            }
        }
    }

    pub(crate) async fn handle_user_activity(&mut self, event: InputEvent) {
        let now = Instant::now();

        self.last_input_at = Some(now);

        debug!("Setting backlight to full: (input activity)");
        self.backlight_controller.set_full();

        match self.ui_state {
            UIState::ShowCook => match event {
                InputEvent::EncoderCCW => {
                    info!("Entering stop-confirm mode: encoder CCW");
                    self.ui_state = UIState::ConfirmStopCooking { last_input_at: now };
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
            UIState::ConfirmStopCooking { .. } => match event {
                InputEvent::EncoderCCW => {
                    self.ui_state = UIState::ConfirmStopCooking { last_input_at: now };
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
    }

    pub(crate) async fn poll_status_if_due(&mut self, stack: embassy_net::Stack<'static>) {
        if self.pending_api_action.is_some() || self.queued_refresh_at.is_some() {
            self.tick += 1;
            return;
        }

        if self.tick % COOK_POLL_INTERVAL == 0 {
            self.update_current_cook(stack).await;
            self.reconcile_current_cook_recipe_title();
            self.sync_status_ui_state();
        }

        if self.tick % RECIPE_POLL_INTERVAL == 0 {
            self.update_recipes(stack).await;
            self.reconcile_current_cook_recipe_title();
        }

        self.update_status(stack).await;

        self.tick += 1;
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
            UIState::ConfirmStopCooking { .. } => {
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

    pub(crate) async fn process_pending_api_action(&mut self, stack: embassy_net::Stack<'static>) {
        let Some(action) = self.pending_api_action.take() else {
            return;
        };

        match action {
            PendingApiAction::Stop => {
                if with_timeout(Duration::from_secs(API_CALL_TIMEOUT_SECS), send_stop(stack))
                    .await
                    .is_err()
                {
                    warn!("POST /stop: timed out");
                }
            }
            PendingApiAction::Start { recipe_id } => {
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
        }
    }

    pub(crate) async fn process_queued_refresh_if_due(
        &mut self,
        stack: embassy_net::Stack<'static>,
    ) {
        if self.pending_api_action.is_some() {
            return;
        }

        if self
            .queued_refresh_at
            .is_some_and(|due_at| Instant::now() >= due_at)
        {
            self.queued_refresh_at = None;

            self.update_status(stack).await;
            self.update_current_cook(stack).await;
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
        self.last_input_at = None;
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
        self.pending_api_action = Some(PendingApiAction::Stop);
        self.queued_refresh_at =
            Some(now + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS));
    }

    fn queue_start_action(&mut self, recipe_id: alloc::string::String, now: Instant) {
        self.pending_api_action = Some(PendingApiAction::Start { recipe_id });
        self.queued_refresh_at =
            Some(now + Duration::from_secs(POST_ACTION_COOK_REFRESH_DELAY_SECS));
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
