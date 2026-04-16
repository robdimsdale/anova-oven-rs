use defmt::{debug, info, warn};
use embassy_time::{with_timeout, Duration, Instant};

use crate::api::{
    fetch_and_log_recipes, fetch_and_log_status, fetch_current_cook, send_start, send_stop,
};
use crate::backlight::BacklightController;
use crate::events::InputEvent;
use crate::lcd::LcdController;
use crate::logic::{is_active_cook, should_dim_backlight};

pub enum UIState {
    ShowIdle,
    ShowCook,
    BrowseRecipes { index: usize },
    ConfirmStopCooking { last_input_at: Instant },
}

const MENU_INACTIVITY_TIMEOUT_SECS: u64 = 15;
const STOP_CONFIRM_TIMEOUT_SECS: u64 = 5;
const LED_DIM_TIMER_SECS: u64 = 5;
const COOK_POLL_INTERVAL: u64 = 10;
const SERVER_RETRY_LOG_INTERVAL: u64 = 5;
const API_CALL_TIMEOUT_SECS: u64 = 3;
const POST_ACTION_COOK_REFRESH_DELAY_SECS: u64 = 1;

// Polling backoff. Without this, repeated failed connections pile up sockets
// in smoltcp and packets in cyw43's tiny rx channel (4 buffers, hardcoded in
// cyw43 0.7.0). Slowing the poll cadence as failures accumulate gives those
// queues time to drain, which is what actually unsticks things — the cyw43
// driver itself can't be safely re-initialized at runtime.
pub(crate) const NORMAL_POLL_INTERVAL_SECS: u64 = 1;
const POLL_BACKOFF_TIER1_FAILS: u64 = 5;
const POLL_BACKOFF_TIER2_FAILS: u64 = 10;
const POLL_BACKOFF_TIER3_FAILS: u64 = 15;
const POLL_BACKOFF_TIER1_SECS: u64 = 5;
const POLL_BACKOFF_TIER2_SECS: u64 = 15;
const POLL_BACKOFF_TIER3_SECS: u64 = 30;

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
    pub(crate) server_online: bool,
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
            server_online: true,
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
        #[allow(static_mut_refs)]
        let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };
        self.current_cook = match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_current_cook(stack, rx_buf),
        )
        .await
        {
            Ok(cook) => cook,
            Err(_) => {
                warn!("GET /current-cook: timed out during init");
                None
            }
        };
        self.latest_status = match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_and_log_status(stack, rx_buf),
        )
        .await
        {
            Ok(status) => status,
            Err(_) => {
                warn!("GET /status: timed out during init");
                None
            }
        };
        self.recipes = match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_and_log_recipes(stack, rx_buf),
        )
        .await
        {
            Ok(recipes) => recipes,
            Err(_) => {
                warn!("GET /recipes: timed out during init");
                alloc::vec::Vec::new()
            }
        };

        self.server_online = self.latest_status.is_some();
        if !self.server_online {
            self.server_fail_count = 1;
            warn!("Server unavailable during init; LCD will show offline status");
        }

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

    fn enter_baseline_state(&mut self) {
        self.ui_state = UIState::ShowIdle;
        self.sync_status_ui_state();
        self.baseline_reentered_at = Some(Instant::now());
        self.last_input_at = None;
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
                        self.apply_optimistic_start_state(&recipe, now);
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
            #[allow(static_mut_refs)]
            let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };
            match with_timeout(
                Duration::from_secs(API_CALL_TIMEOUT_SECS),
                fetch_current_cook(stack, rx_buf),
            )
            .await
            {
                Ok(cook) => {
                    self.current_cook = cook;
                }
                Err(_) => {
                    warn!("GET /current-cook: timed out");
                }
            }
            self.reconcile_current_cook_recipe_title();
            self.sync_status_ui_state();
        }

        #[allow(static_mut_refs)]
        let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };
        match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_and_log_status(stack, rx_buf),
        )
        .await
        {
            Ok(Some(status)) => {
                if !self.server_online {
                    info!("Server communication restored");
                }
                self.server_online = true;
                self.server_fail_count = 0;
                self.latest_status = Some(status);
                self.sync_status_ui_state();
            }
            Ok(None) => {
                self.server_fail_count = self.server_fail_count.saturating_add(1);
                if self.server_online {
                    warn!("Lost communication with server");
                } else if self.server_fail_count % SERVER_RETRY_LOG_INTERVAL == 0 {
                    warn!(
                        "Still offline: {} consecutive status poll failures",
                        self.server_fail_count
                    );
                }

                self.server_online = false;
                // Clear stale data so the UI reflects that data is unavailable.
                self.latest_status = None;
                self.current_cook = None;
                self.sync_status_ui_state();
            }
            Err(_) => {
                warn!("GET /status: timed out");
                self.server_fail_count = self.server_fail_count.saturating_add(1);
                if self.server_online {
                    warn!("Lost communication with server");
                } else if self.server_fail_count % SERVER_RETRY_LOG_INTERVAL == 0 {
                    warn!(
                        "Still offline: {} consecutive status poll failures",
                        self.server_fail_count
                    );
                }

                self.server_online = false;
                self.latest_status = None;
                self.current_cook = None;
                self.sync_status_ui_state();
            }
        }

        self.tick += 1;
    }

    pub(crate) async fn render_current_view(&mut self) {
        self.apply_backlight_policy();

        if !self.server_online {
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

        #[allow(static_mut_refs)]
        let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };

        match action {
            PendingApiAction::Stop => {
                if with_timeout(
                    Duration::from_secs(API_CALL_TIMEOUT_SECS),
                    send_stop(stack, rx_buf),
                )
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
                    send_start(stack, rx_buf, recipe_id.as_str()),
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
            self.refresh_current_cook_and_status_now(stack).await;
        }
    }

    fn apply_backlight_policy(&mut self) {
        if !self.server_online {
            debug!("Setting backlight to full: (server offline)");
            self.backlight_controller.set_full();
            return;
        }

        let active_cook = is_active_cook(
            self.current_cook.is_some(),
            self.latest_status
                .as_ref()
                .map(|status| status.mode.as_str()),
        );
        let baseline_elapsed_secs = self.baseline_reentered_at.map(|t| t.elapsed().as_secs());
        let should_dim = should_dim_backlight(
            self.is_status_view(),
            baseline_elapsed_secs,
            active_cook,
            LED_DIM_TIMER_SECS,
        );

        if should_dim {
            self.backlight_controller.set_dim();
            debug!("Setting backlight to dim: (idle baseline)");
        } else if active_cook || !self.is_status_view() {
            debug!("Setting backlight to full: (active state)");
            self.backlight_controller.set_full();
        }
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
            status.mode = alloc::string::String::from("idle");
            status.timer_mode = alloc::string::String::from("idle");
            status.timer_current_secs = 0;
            status.target_temperature_c = None;
            status.steam_target_pct = None;
        }

        self.sync_status_ui_state();
    }

    fn apply_optimistic_start_state(&mut self, recipe: &anova_oven_api::Recipe, now: Instant) {
        let cook_stage_count = recipe
            .stages
            .iter()
            .filter(|stage| stage.kind.as_str() == "cook")
            .count();

        self.current_cook = Some(anova_oven_api::CurrentCook {
            recipe_title: recipe.title.clone(),
            recipe_id: Some(recipe.id.clone()),
            started_at: alloc::string::String::from("pending"),
            stages: recipe.stages.clone(),
            cook_stage_count,
            total_stage_count: recipe.stages.len(),
        });

        self.ui_state = UIState::ShowCook;
        self.baseline_reentered_at = Some(now);
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

    async fn refresh_current_cook_and_status_now(&mut self, stack: embassy_net::Stack<'static>) {
        #[allow(static_mut_refs)]
        let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };

        match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_current_cook(stack, rx_buf),
        )
        .await
        {
            Ok(cook) => {
                self.current_cook = cook;
                self.reconcile_current_cook_recipe_title();
                self.sync_status_ui_state();
            }
            Err(_) => {
                warn!("GET /current-cook: timed out after action");
            }
        }

        match with_timeout(
            Duration::from_secs(API_CALL_TIMEOUT_SECS),
            fetch_and_log_status(stack, rx_buf),
        )
        .await
        {
            Ok(Some(status)) => {
                if !self.server_online {
                    info!("Server communication restored");
                }
                self.server_online = true;
                self.server_fail_count = 0;
                self.latest_status = Some(status);
                self.sync_status_ui_state();
            }
            Ok(None) => {
                warn!("GET /status: failed after action");
            }
            Err(_) => {
                warn!("GET /status: timed out after action");
            }
        }
    }

    fn sync_status_ui_state(&mut self) {
        if !matches!(self.ui_state, UIState::ShowIdle | UIState::ShowCook) {
            return;
        }

        let active_cook = is_active_cook(
            self.current_cook.is_some(),
            self.latest_status
                .as_ref()
                .map(|status| status.mode.as_str()),
        );

        self.ui_state = if active_cook {
            UIState::ShowCook
        } else {
            UIState::ShowIdle
        };
    }
}
