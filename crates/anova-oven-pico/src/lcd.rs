use hd44780_driver::non_blocking::{Cursor, CursorBlink, Display, DisplayMode, HD44780};

use alloc::string::String;
use alloc::vec::Vec;
use embassy_rp::gpio::Output;
use embassy_time::{Delay, Duration, Instant};

const LCD_WIDTH: usize = 16;
const SCROLL_STEP_MS: u64 = 350;
const CHAR_SCROLL_COUNT: usize = 3;
const END_PAUSE_MS: u64 = 1200;
// Minimum time to display a slot whose content fits on-screen without scrolling.
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

    pub(crate) async fn configure(&mut self) {
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
        self.row1_scroll_state.as_ref().map_or(false, |s| {
            s.cycle_complete && Instant::now() >= s.pause_until
        })
    }

    pub(crate) async fn render_wifi_init(&mut self) {
        self.write_row(0, "Anova Oven", 0).await;
        self.write_row(1, "Init: WIFI...", 0).await;
    }

    pub(crate) async fn render_dhcp_init(&mut self) {
        self.write_row(0, "Anova Oven", 0).await;
        self.write_row(1, "Init: DHCP...", 0).await;
    }

    pub(crate) async fn render_server_offline(&mut self, tick: u64) {
        self.write_row(0, "Server Offline", tick).await;
        self.write_row(1, "Check backend", tick).await;
    }

    pub(crate) async fn render_status_display(
        &mut self,
        tick: u64,
        status: Option<&anova_oven_api::OvenStatus>,
        current_cook: Option<&anova_oven_api::CurrentCook>,
    ) {
        let Some(status) = status else {
            self.write_row(0, "", tick).await;
            self.write_row(1, "Status: N/A", tick).await;
            return;
        };

        let is_cooking = current_cook.is_some() || status.is_cooking();

        if let Some(cook) = current_cook {
            let name = cook.display_name();
            self.write_row(0, name, tick).await;

            let current_stage = cook.current_stage(status);
            let phase = status.phase();
            let stage_title = current_stage.and_then(|s| s.title.as_deref());
            let show_phase = stage_title.is_some_and(|title| !title.eq_ignore_ascii_case(phase));
            let has_timer_or_probe =
                status.timer_remaining_secs().is_some() || status.probe_temperature_c.is_some();

            let num_items: u64 = 2 + u64::from(show_phase) + u64::from(has_timer_or_probe);

            // Advance slot only after the current slot's animation is done.
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

            if slot == slot_idx {
                let row1 = match stage_title {
                    Some(title) => alloc::format!("Stage: {title}"),
                    None if cook.recipe_title == "[manual]" => {
                        alloc::string::String::from("Manual stage")
                    }
                    None => alloc::format!("Stage: {phase}"),
                };
                self.write_row(1, &row1, tick).await;
            }
            slot_idx += 1;

            if slot == slot_idx {
                let current_f = celcius_to_fahrenheit(status.current_temperature_c());
                let mut row1 = alloc::format!("{:.0}F", current_f);
                if let Some(target_c) = status.target_temperature_c {
                    let target_f = celcius_to_fahrenheit(target_c);
                    row1.push_str(&alloc::format!(">{:.0}F", target_f));
                }
                self.write_row(1, &row1, tick).await;
            }
            slot_idx += 1;

            if show_phase {
                if slot == slot_idx {
                    let row1 = alloc::format!("Phase: {phase}");
                    self.write_row(1, &row1, tick).await;
                }
                slot_idx += 1;
            }

            if has_timer_or_probe && slot == slot_idx {
                if let Some(remaining) = status.timer_remaining_secs() {
                    let h = remaining / 3600;
                    let m = (remaining % 3600) / 60;
                    let s = remaining % 60;
                    let row1 = if h > 0 {
                        alloc::format!("Timer: {h}:{m:02}:{s:02}")
                    } else {
                        alloc::format!("Timer: {m:02}:{s:02}")
                    };
                    self.write_row(1, &row1, tick).await;
                } else if let Some(probe_c) = status.probe_temperature_c {
                    let probe_f = celcius_to_fahrenheit(probe_c);
                    let mut row1 = alloc::format!("P:{:.0}F", probe_f);
                    if let Some(target_c) = current_stage.and_then(|st| st.probe_target_c) {
                        let target_f = celcius_to_fahrenheit(target_c);
                        row1.push_str(&alloc::format!(">{:.0}F", target_f));
                    }
                    self.write_row(1, &row1, tick).await;
                }
            }
        } else if is_cooking {
            self.write_row(0, "Manual cook", tick).await;

            let row1 = if let Some(steam) = status.steam_target_pct {
                alloc::format!("{} S:{:.0}%", status.phase(), steam)
            } else {
                alloc::string::String::from(status.phase())
            };
            self.write_row(1, &row1, tick).await;
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
            self.write_row(1, &row1, tick).await;
        }
    }

    pub(crate) async fn render_recipe_browser(
        &mut self,
        recipes: &[anova_oven_api::Recipe],
        index: usize,
        tick: u64,
    ) {
        if recipes.is_empty() {
            self.write_row(0, "No recipes", tick).await;
            self.write_row(1, "", tick).await;
            return;
        }

        let header = alloc::format!("Recipe {}/{}", index + 1, recipes.len());
        self.write_row(0, &header, tick).await;
        self.write_row(1, &recipes[index].title, tick).await;
    }

    pub(crate) async fn render_stop_confirmation(
        &mut self,
        tick: u64,
        status: Option<&anova_oven_api::OvenStatus>,
        current_cook: Option<&anova_oven_api::CurrentCook>,
    ) {
        if let Some(cook) = current_cook {
            self.write_row(0, cook.display_name(), tick).await;
        } else if let Some(status) = status {
            self.write_row(0, status.phase(), tick).await;
        } else {
            self.write_row(0, "Active cook", tick).await;
        }

        self.write_row(1, "Stop cooking?", tick).await;
    }

    async fn write_row(&mut self, row: u8, text: &str, _tick: u64) {
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
                    // Pause at the right edge before wrapping to the start.
                    state.pause_until = now + Duration::from_millis(END_PAUSE_MS);
                    // Signal done; slot advancement waits for pause_until to elapse.
                    state.cycle_complete = true;
                }
            } else {
                state.offset = 0;
                changed = true;
                // Keep the existing pause at the left edge after wrapping.
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

pub fn celcius_to_fahrenheit(c: f32) -> f32 {
    c * 1.8 + 32.0
}
