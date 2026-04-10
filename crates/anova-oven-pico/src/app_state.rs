use defmt::{debug, info};
use embassy_time::{Duration, Instant};

use crate::api::{fetch_and_log_recipes, fetch_and_log_status, fetch_current_cook, send_stop};
use crate::backlight::BacklightController;
use crate::display::LcdController;
use crate::events::{handle_input_event, InputEvent, UIState};
use crate::logic::{is_active_cook, should_dim_backlight};

const MENU_INACTIVITY_TIMEOUT_SECS: u64 = 15;
const LED_DIM_TIMER_SECS: u64 = 5;
const COOK_POLL_INTERVAL: u64 = 10;

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
        self.current_cook = fetch_current_cook(stack, rx_buf).await;
        self.latest_status = fetch_and_log_status(stack, rx_buf).await;
        self.recipes = fetch_and_log_recipes(stack, rx_buf).await;
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
            send_stop(stack, rx_buf).await;
        }

        handle_input_event(event, &mut self.ui_state, &self.recipes);
    }

    pub(crate) async fn poll_status_if_due(&mut self, stack: embassy_net::Stack<'static>) {
        if self.tick % COOK_POLL_INTERVAL == 0 {
            #[allow(static_mut_refs)]
            let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };
            self.current_cook = fetch_current_cook(stack, rx_buf).await;
        }

        #[allow(static_mut_refs)]
        let rx_buf = unsafe { &mut crate::HTTP_RX_BUF };
        if let Some(status) = fetch_and_log_status(stack, rx_buf).await {
            if status.mode == "idle" && self.current_cook.is_some() {
                self.current_cook = None;
            }
            self.latest_status = Some(status);
        }

        self.tick += 1;
    }

    pub(crate) async fn render_current_view(&mut self) {
        self.apply_backlight_policy();
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
}
