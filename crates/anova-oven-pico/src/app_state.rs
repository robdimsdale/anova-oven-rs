use embassy_time::{Delay, Duration, Instant};
use hd44780_driver::non_blocking::HD44780;

use crate::api::{fetch_and_log_status, fetch_current_cook};
use crate::backlight::BacklightController;
use crate::display::{render_recipe_browser, render_status_display};
use crate::events::{handle_input_event, UIState};
use crate::logic::{is_active_cook, should_dim_backlight};

pub(crate) struct AppState {
    pub(crate) tick: u64,
    pub(crate) ui_state: UIState,
    pub(crate) last_input_at: Option<Instant>,
    pub(crate) baseline_reentered_at: Option<Instant>,
    pub(crate) backlight_dimmed: bool,
    pub(crate) current_cook: Option<anova_oven_api::CurrentCook>,
    pub(crate) latest_status: Option<anova_oven_api::OvenStatus>,
}

impl AppState {
    pub(crate) fn new(
        current_cook: Option<anova_oven_api::CurrentCook>,
        latest_status: Option<anova_oven_api::OvenStatus>,
    ) -> Self {
        Self {
            tick: 0,
            ui_state: UIState::ShowStatus,
            last_input_at: None,
            baseline_reentered_at: Some(Instant::now()),
            backlight_dimmed: true,
            current_cook,
            latest_status,
        }
    }

    pub(crate) fn update_inactivity_timeout(&mut self) {
        if let Some(t) = self.last_input_at {
            if t.elapsed() >= Duration::from_secs(crate::INACTIVITY_TIMEOUT_SECS) {
                if !matches!(self.ui_state, UIState::ShowStatus) {
                    defmt::info!("Inactivity timeout: reverting to ShowStatus");
                    self.ui_state = UIState::ShowStatus;
                    self.baseline_reentered_at = Some(Instant::now());
                }
                self.last_input_at = None;
            }
        }
    }

    pub(crate) fn handle_user_activity(
        &mut self,
        event: crate::events::InputEvent,
        recipes: &[anova_oven_api::Recipe],
        backlight: &mut BacklightController,
    ) {
        self.last_input_at = Some(Instant::now());
        self.baseline_reentered_at = None;

        if self.backlight_dimmed {
            backlight.set_full();
            self.backlight_dimmed = false;
            defmt::info!("Backlight: 100% (input activity)");
        }

        handle_input_event(event, &mut self.ui_state, recipes);
    }

    pub(crate) async fn poll_status_if_due(&mut self, stack: embassy_net::Stack<'static>) {
        if self.tick % crate::COOK_POLL_INTERVAL == 0 {
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

    pub(crate) fn apply_backlight_policy(&mut self, backlight: &mut BacklightController) {
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
            crate::LED_DIM_TIMER_SECS,
        );

        if should_dim && !self.backlight_dimmed {
            backlight.set_dim();
            self.backlight_dimmed = true;
            defmt::info!("Backlight: dim (idle baseline)");
        } else if (active_cook || !matches!(self.ui_state, UIState::ShowStatus))
            && self.backlight_dimmed
        {
            backlight.set_full();
            self.backlight_dimmed = false;
            defmt::info!("Backlight: full (active state)");
        }
    }

    pub(crate) async fn render_current_view(
        &self,
        lcd: &mut HD44780<
            impl hd44780_driver::non_blocking::bus::DataBus,
            impl hd44780_driver::memory_map::DisplayMemoryMap,
            impl hd44780_driver::charset::CharsetWithFallback,
        >,
        delay: &mut Delay,
        recipes: &[anova_oven_api::Recipe],
    ) {
        match &self.ui_state {
            UIState::ShowStatus => {
                render_status_display(
                    lcd,
                    delay,
                    self.tick,
                    self.latest_status.as_ref(),
                    self.current_cook.as_ref(),
                )
                .await;
            }
            UIState::BrowseRecipes { index } => {
                render_recipe_browser(lcd, delay, recipes, *index).await;
            }
        }
    }
}
