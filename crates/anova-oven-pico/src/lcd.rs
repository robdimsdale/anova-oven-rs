use hd44780_driver::non_blocking::{Cursor, CursorBlink, Display, DisplayMode, HD44780};

use alloc::string::String;
use alloc::vec::Vec;
use embassy_rp::gpio::Output;
use embassy_time::{Delay, Duration, Instant};

use anova_oven_pico_core::view_plan::recipe_browser_header;

use crate::api::celcius_to_fahrenheit;
use crate::display::{DisplayBackend, ViewSpec};

const LCD_WIDTH: usize = 16;
const SCROLL_STEP_MS: u64 = 350;
const CHAR_SCROLL_COUNT: usize = 3;
const END_PAUSE_MS: u64 = 1200;
const MIN_SLOT_HOLD_MS: u64 = 3000;

type LcdBus = hd44780_driver::non_blocking::bus::FourBitBus<
    Output<'static>,
    Output<'static>,
    Output<'static>,
    Output<'static>,
    Output<'static>,
    Output<'static>,
>;
type LcdMemoryMap = hd44780_driver::memory_map::MemoryMap1602;
type LcdCharset = hd44780_driver::charset::EmptyFallback<hd44780_driver::charset::CharsetUniversal>;
type LcdDriver = HD44780<LcdBus, LcdMemoryMap, LcdCharset>;

pub(crate) struct LcdController {
    lcd: LcdDriver,
    delay: Delay,
    row0_scroll_state: Option<RowScrollState>,
    row1_scroll_state: Option<RowScrollState>,
    row0_last_rendered: Option<String>,
    row1_last_rendered: Option<String>,
    row1_slot: Option<u64>,
}

struct RowScrollState {
    text: String,
    offset: usize,
    last_step_at: Instant,
    pause_until: Instant,
    shown_at: Instant,
    cycle_complete: bool,
}

impl LcdController {
    pub(crate) fn new(lcd: LcdDriver, delay: Delay) -> Self {
        Self {
            lcd,
            delay,
            row0_scroll_state: None,
            row1_scroll_state: None,
            row0_last_rendered: None,
            row1_last_rendered: None,
            row1_slot: None,
        }
    }

    async fn configure_lcd(&mut self) {
        self.lcd
            .set_display_mode(
                DisplayMode {
                    cursor_visibility: Cursor::Invisible,
                    cursor_blink: CursorBlink::Off,
                    display: Display::On,
                },
                &mut self.delay,
            )
            .await
            .ok();
        self.lcd.reset(&mut self.delay).await.ok();
        self.lcd.clear(&mut self.delay).await.ok();
    }

    /// Returns true when the current row-1 slot has finished displaying:
    /// long text: after completing one full scroll AND the end-pause has elapsed;
    /// short text: after MIN_SLOT_HOLD_MS has elapsed.
    fn row1_animation_done(&self) -> bool {
        self.row1_scroll_state
            .as_ref()
            .is_some_and(|state| state.cycle_complete && Instant::now() >= state.pause_until)
    }

    async fn render_lcd(&mut self, view: &ViewSpec) {
        match view {
            ViewSpec::WifiInit => {
                self.write_row(0, "Anova Oven").await;
                self.write_row(1, "Init: WIFI...").await;
            }
            ViewSpec::DhcpInit => {
                self.write_row(0, "Anova Oven").await;
                self.write_row(1, "Init: DHCP...").await;
            }
            ViewSpec::Connecting => {
                self.write_row(0, "Anova Oven").await;
                self.write_row(1, "Connecting...").await;
            }
            ViewSpec::ServerOffline => {
                self.write_row(0, "Server Offline").await;
                self.write_row(1, "Check backend").await;
            }
            ViewSpec::UpstreamStale { disconnected_secs } => {
                use core::fmt::Write as _;
                self.write_row(0, "Anova Link Down").await;
                // By the time we're here disconnected_secs >= the grace window
                // (60s), so minutes >= 1. write_row scrolls if it overflows 16.
                let mut row1: heapless::String<32> = heapless::String::new();
                let _ = write!(row1, "Stale {}m - check", disconnected_secs / 60);
                self.write_row(1, row1.as_str()).await;
            }
            ViewSpec::Status { status, cook, .. } => {
                // The age of the fetch, not the fetch itself, is what advances
                // the timer row between polls — see `render_status_display`.
                self.render_status_display(
                    status.as_ref(),
                    cook.as_ref(),
                    view.status_age_secs(Instant::now()),
                )
                .await;
            }
            ViewSpec::RecipeBrowser {
                source,
                count,
                index,
                title,
            } => {
                self.render_recipe_browser(*source, *count, *index, title)
                    .await;
            }
            ViewSpec::StopConfirmation { status, cook } => {
                self.render_stop_confirmation(status.as_ref(), cook.as_ref())
                    .await;
            }
            ViewSpec::StartingCook { recipe_title } => {
                self.write_row(0, recipe_title).await;
                self.write_row(1, "Starting...").await;
            }
            ViewSpec::NextStagePrompt { recipe_title } => {
                let row0 = if recipe_title.is_empty() {
                    "Active cook"
                } else {
                    recipe_title.as_str()
                };
                self.write_row(0, row0).await;
                self.write_row(1, "Next stage ready").await;
            }
            ViewSpec::Recovery {
                reset_count,
                panic_count,
                message,
            } => {
                self.render_recovery(*reset_count, *panic_count, message.as_deref())
                    .await;
            }
        }
    }

    async fn render_recovery(&mut self, reset_count: u32, panic_count: u32, message: Option<&str>) {
        use core::fmt::Write as _;
        let mut row0: heapless::String<32> = heapless::String::new();
        let label = if panic_count > 0 { "Panic" } else { "Reset" };
        let _ = write!(row0, "{label} p={panic_count} r={reset_count}");
        self.write_row(0, row0.as_str()).await;
        match message {
            Some(msg) if !msg.is_empty() => self.write_row(1, msg).await,
            _ => self.write_row(1, "no msg").await,
        }
    }

    /// `status_age_secs` is how long ago `status` was fetched: the timer row
    /// counts down from there rather than showing the last number the server
    /// sent, so it ticks every second instead of once per poll.
    async fn render_status_display(
        &mut self,
        status: Option<&anova_oven_api::OvenStatus>,
        current_cook: Option<&anova_oven_api::CurrentCook>,
        status_age_secs: u64,
    ) {
        let Some(status) = status else {
            self.write_row(0, "").await;
            self.write_row(1, "Status: N/A").await;
            return;
        };

        let is_cooking = current_cook.is_some() || status.is_cooking();

        if let Some(cook) = current_cook {
            self.write_row(0, cook.display_name()).await;

            #[allow(deprecated)]
            let current_stage = cook.current_stage(status);
            let phase = status.phase();
            let stage_title = current_stage.and_then(|stage| stage.title.as_deref());
            let show_phase = stage_title.is_some_and(|title| !title.eq_ignore_ascii_case(phase));
            let remaining_secs = status.timer_remaining_secs_after(status_age_secs);
            let has_timer_or_probe =
                remaining_secs.is_some() || status.probe_temperature_c.is_some();

            // A manual cook has no recipe, so every stage in it is manual and
            // naming one tells the user nothing they can't see from row 0's
            // "Manual cook". Drop the slot rather than spend a rotation on it.
            let stage_row = match stage_title {
                Some(title) => Some(alloc::format!("Stage: {title}")),
                None if cook.recipe_title == "[manual]" => None,
                None => Some(alloc::format!("Stage: {phase}")),
            };

            let num_items: u64 = 1
                + u64::from(stage_row.is_some())
                + u64::from(show_phase)
                + u64::from(has_timer_or_probe);

            if self.row1_animation_done() {
                let current = self.row1_slot.unwrap_or(0);
                let next = (current + 1) % num_items;
                self.row1_slot = Some(next);
                self.row1_scroll_state = None;
                self.row1_last_rendered = None;
            } else if self.row1_slot.is_none() {
                self.row1_slot = Some(0);
            }
            let slot = self.row1_slot.unwrap_or(0).min(num_items - 1);
            let mut slot_idx = 0;

            if let Some(stage_row) = stage_row {
                if slot == slot_idx {
                    self.write_row(1, &stage_row).await;
                }
                slot_idx += 1;
            }

            if slot == slot_idx {
                let current_f = celcius_to_fahrenheit(status.current_temperature_c());
                let mut row1 = alloc::format!("{:.0}F", current_f);
                if let Some(target_c) = status.target_temperature_c {
                    let target_f = celcius_to_fahrenheit(target_c);
                    row1.push_str(&alloc::format!(">{:.0}F", target_f));
                }
                self.write_row(1, &row1).await;
            }
            slot_idx += 1;

            if show_phase {
                if slot == slot_idx {
                    let row1 = alloc::format!("Phase: {phase}");
                    self.write_row(1, &row1).await;
                }
                slot_idx += 1;
            }

            if has_timer_or_probe && slot == slot_idx {
                if let Some(remaining) = remaining_secs {
                    let h = remaining / 3600;
                    let m = (remaining % 3600) / 60;
                    let s = remaining % 60;
                    let row1 = if h > 0 {
                        alloc::format!("Timer: {h}:{m:02}:{s:02}")
                    } else {
                        alloc::format!("Timer: {m:02}:{s:02}")
                    };
                    self.write_row(1, &row1).await;
                } else if let Some(probe_c) = status.probe_temperature_c {
                    let probe_f = celcius_to_fahrenheit(probe_c);
                    let mut row1 = alloc::format!("P:{:.0}F", probe_f);
                    if let Some(target_c) = current_stage.and_then(|stage| stage.probe_target_c) {
                        let target_f = celcius_to_fahrenheit(target_c);
                        row1.push_str(&alloc::format!(">{:.0}F", target_f));
                    }
                    self.write_row(1, &row1).await;
                }
            }
        } else if is_cooking {
            self.write_row(0, "Manual cook").await;

            let row1 = if let Some(steam) = status.steam_target_pct {
                alloc::format!("{} S:{:.0}%", status.phase(), steam)
            } else {
                String::from(status.phase())
            };
            self.write_row(1, &row1).await;
        } else {
            // Row 0 is rendered via direct LCD byte writes below (for degree glyph),
            // so invalidate cached state to keep transition redraws correct.
            self.row0_scroll_state = None;
            self.row0_last_rendered = None;

            self.lcd.set_cursor_xy((0, 0), &mut self.delay).await.ok();
            let temp_str = alloc::format!(
                "{:.0}",
                celcius_to_fahrenheit(status.current_temperature_c())
            );
            let mut row0_len = temp_str.len() + 2;
            self.lcd.write_str(&temp_str, &mut self.delay).await.ok();
            self.lcd.write_byte(0xDF, &mut self.delay).await.ok();
            self.lcd.write_str("F", &mut self.delay).await.ok();
            if let Some(probe_c) = status.probe_temperature_c {
                let probe_str = alloc::format!(" P:{:.0}", celcius_to_fahrenheit(probe_c));
                row0_len += probe_str.len() + 2;
                self.lcd.write_str(&probe_str, &mut self.delay).await.ok();
                self.lcd.write_byte(0xDF, &mut self.delay).await.ok();
                self.lcd.write_str("F", &mut self.delay).await.ok();
            }
            for _ in row0_len..LCD_WIDTH {
                self.lcd.write_byte(b' ', &mut self.delay).await.ok();
            }

            let row1 = if let Some(steam) = status.steam_target_pct {
                alloc::format!("{} S:{:.0}%", status.mode, steam)
            } else {
                status.mode.clone()
            };
            self.write_row(1, &row1).await;
        }
    }

    async fn render_recipe_browser(
        &mut self,
        source: anova_oven_api::RecipeSource,
        count: usize,
        index: usize,
        title: &str,
    ) {
        if count == 0 {
            self.write_row(0, "No recipes").await;
            self.write_row(1, "").await;
            return;
        }

        let header = recipe_browser_header(source, index, count);
        self.write_row(0, &header).await;
        self.write_row(1, title).await;
    }

    async fn render_stop_confirmation(
        &mut self,
        status: Option<&anova_oven_api::OvenStatus>,
        current_cook: Option<&anova_oven_api::CurrentCook>,
    ) {
        if let Some(cook) = current_cook {
            self.write_row(0, cook.display_name()).await;
        } else if let Some(status) = status {
            self.write_row(0, status.phase()).await;
        } else {
            self.write_row(0, "Active cook").await;
        }

        self.write_row(1, "Stop cooking?").await;
    }

    async fn write_row(&mut self, row: u8, text: &str) {
        let now = Instant::now();
        let row_state = self.row_scroll_state_mut(row);

        let text_changed = row_state
            .as_ref()
            .is_none_or(|state| state.text.as_str() != text);

        if text_changed {
            *row_state = Some(RowScrollState {
                text: text.into(),
                offset: 0,
                last_step_at: now,
                pause_until: now + Duration::from_millis(END_PAUSE_MS),
                shown_at: now,
                cycle_complete: false,
            });
        }

        let is_scrolling = text.chars().count() > LCD_WIDTH;
        let (visible, step_changed) = if let Some(state) = row_state.as_mut() {
            Self::visible_window(text, state, now)
        } else {
            Self::visible_window(
                text,
                &mut RowScrollState {
                    text: text.into(),
                    offset: 0,
                    last_step_at: now,
                    pause_until: now,
                    shown_at: now,
                    cycle_complete: false,
                },
                now,
            )
        };

        if is_scrolling && !text_changed && !step_changed {
            return;
        }

        let mut rendered = visible;
        while rendered.len() < LCD_WIDTH {
            rendered.push(' ');
        }

        let last_rendered = self.row_last_rendered_mut(row);
        if last_rendered.as_deref() == Some(rendered.as_str()) {
            return;
        }

        *last_rendered = Some(rendered.clone());

        self.lcd.set_cursor_xy((0, row), &mut self.delay).await.ok();
        let len = rendered.len().min(LCD_WIDTH);
        self.lcd
            .write_str(&rendered[..len], &mut self.delay)
            .await
            .ok();
        for _ in len..LCD_WIDTH {
            self.lcd.write_byte(b' ', &mut self.delay).await.ok();
        }
    }

    fn visible_window(text: &str, state: &mut RowScrollState, now: Instant) -> (String, bool) {
        let chars: Vec<char> = text.chars().collect();
        let len = chars.len();
        if len <= LCD_WIDTH {
            state.offset = 0;
            state.last_step_at = now;
            state.pause_until = now;
            if now.duration_since(state.shown_at).as_millis() >= MIN_SLOT_HOLD_MS {
                state.cycle_complete = true;
            }
            return (text.into(), true);
        }

        let overflow = len - LCD_WIDTH;
        let mut changed = false;

        // Keep marquee smooth: never "catch up" by multiple chars after delays.
        if now >= state.pause_until
            && now.duration_since(state.last_step_at).as_millis() >= SCROLL_STEP_MS
        {
            state.last_step_at = now;
            if state.offset < overflow {
                state.offset += CHAR_SCROLL_COUNT;
                changed = true;
                if state.offset >= overflow {
                    state.offset = overflow;
                    state.pause_until = now + Duration::from_millis(END_PAUSE_MS);
                    state.cycle_complete = true;
                }
            } else {
                state.offset = 0;
                changed = true;
                state.pause_until = now + Duration::from_millis(END_PAUSE_MS);
            }
        }

        (
            chars[state.offset..state.offset + LCD_WIDTH]
                .iter()
                .collect(),
            changed,
        )
    }

    fn row_scroll_state_mut(&mut self, row: u8) -> &mut Option<RowScrollState> {
        if row == 0 {
            &mut self.row0_scroll_state
        } else {
            &mut self.row1_scroll_state
        }
    }

    fn row_last_rendered_mut(&mut self, row: u8) -> &mut Option<String> {
        if row == 0 {
            &mut self.row0_last_rendered
        } else {
            &mut self.row1_last_rendered
        }
    }
}

impl DisplayBackend for LcdController {
    async fn configure(&mut self) {
        self.configure_lcd().await;
    }

    async fn render(&mut self, view: &ViewSpec) {
        self.render_lcd(view).await;
    }
}
