use defmt::{debug, info, warn};
use embassy_time::{with_timeout, Duration, Instant};

use crate::api::{
    fetch_and_log_recipes, fetch_and_log_status, fetch_current_cook, send_start, send_stop,
};
use crate::backlight::BacklightController;
use crate::display::LcdController;
use crate::events::{handle_input_event, InputEvent, UIState};
use crate::logic::{is_active_cook, should_dim_backlight};

const MENU_INACTIVITY_TIMEOUT_SECS: u64 = 15;
const LED_DIM_TIMER_SECS: u64 = 5;
const COOK_POLL_INTERVAL: u64 = 10;
const SERVER_RETRY_LOG_INTERVAL: u64 = 5;
const API_CALL_TIMEOUT_SECS: u64 = 3;

pub(crate) struct AppState<DB, MM, CH>
where
    DB: hd44780_driver::non_blocking::bus::DataBus,
    MM: hd44780_driver::memory_map::DisplayMemoryMap,
    CH: hd44780_driver::charset::CharsetWithFallback,
{
    pub(crate) tick: u64,
    pub(crate) ui_state: UIState,
    pub(crate) last_input_at: Option<Instant>,
    pub(crate) baseline_reentered_at: Option<Instant>,
    pub(crate) current_cook: Option<anova_oven_api::CurrentCook>,
    pub(crate) latest_status: Option<anova_oven_api::OvenStatus>,
    pub(crate) recipes: alloc::vec::Vec<anova_oven_api::Recipe>,
    pub(crate) server_online: bool,
    pub(crate) server_fail_count: u64,

    backlight_controller: BacklightController,
    lcd_controller: LcdController<DB, MM, CH>,
}

impl<DB, MM, CH> AppState<DB, MM, CH>
where
    DB: hd44780_driver::non_blocking::bus::DataBus,
    MM: hd44780_driver::memory_map::DisplayMemoryMap,
    CH: hd44780_driver::charset::CharsetWithFallback,
{
    pub(crate) fn new(
        backlight_controller: BacklightController,
        lcd_controller: LcdController<DB, MM, CH>,
    ) -> Self {
        Self {
            tick: 0,
            ui_state: UIState::ShowStatus,
            last_input_at: None,
            baseline_reentered_at: Some(Instant::now()),
            current_cook: None,
            latest_status: None,
            recipes: alloc::vec::Vec::new(),
            server_online: true,
            server_fail_count: 0,
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
    }

    pub(crate) fn update_inactivity_timeout(&mut self) {
        if let Some(t) = self.last_input_at {
            if t.elapsed() >= Duration::from_secs(MENU_INACTIVITY_TIMEOUT_SECS) {
                if !matches!(self.ui_state, UIState::ShowStatus) {
                    info!("Inactivity timeout: reverting to ShowStatus");
                    self.ui_state = UIState::ShowStatus;
                    self.baseline_reentered_at = Some(Instant::now());
                }
                self.last_input_at = None;
            }
        }
    }

    pub(crate) async fn handle_user_activity(
        &mut self,
        event: InputEvent,
        stack: embassy_net::Stack<'static>,
    ) {
        self.last_input_at = Some(Instant::now());
        self.baseline_reentered_at = None;

        debug!("Setting backlight to full: (input activity)");
        self.backlight_controller.set_full();

        if matches!(event, InputEvent::StopButton) {
            info!("Sending POST /stop");
            #[allow(static_mut_refs)]
            let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };
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

        if matches!(event, InputEvent::EncoderButton) {
            if let UIState::BrowseRecipes { index } = self.ui_state {
                if let Some(recipe) = self.recipes.get(index) {
                    info!("Sending POST /start with recipe: {}", recipe.title.as_str());
                    #[allow(static_mut_refs)]
                    let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };
                    if with_timeout(
                        Duration::from_secs(API_CALL_TIMEOUT_SECS),
                        send_start(stack, rx_buf, recipe.id.as_str()),
                    )
                    .await
                    .is_err()
                    {
                        warn!("POST /start: timed out");
                    }
                }
            }
        }

        handle_input_event(event, &mut self.ui_state, &self.recipes);
    }

    pub(crate) async fn poll_status_if_due(&mut self, stack: embassy_net::Stack<'static>) {
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
            UIState::ShowStatus => {
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
            matches!(self.ui_state, UIState::ShowStatus),
            baseline_elapsed_secs,
            active_cook,
            LED_DIM_TIMER_SECS,
        );

        if should_dim {
            self.backlight_controller.set_dim();
            debug!("Setting backlight to dim: (idle baseline)");
        } else if active_cook || !matches!(self.ui_state, UIState::ShowStatus) {
            debug!("Setting backlight to full: (active state)");
            self.backlight_controller.set_full();
        }
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
}
