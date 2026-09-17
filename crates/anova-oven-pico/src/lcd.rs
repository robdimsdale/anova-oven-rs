//! `LcdController` — the `ui-lcd` display backend: drives the 16x2 HD44780
//! character LCD over its 4-bit parallel bus.
//!
//! Layout, slot rotation and marquee timing all live in
//! [`anova_oven_pico_core::lcd_plan`], which is pure and host-tested — timing
//! included, since [`LcdAnimator::tick`] is handed the clock rather than
//! reading it. What is left here is the panel itself: move the cursor and
//! strobe the bytes for whichever rows a tick reports as changed.

use hd44780_driver::non_blocking::{Cursor, CursorBlink, Display, DisplayMode, HD44780};

use embassy_rp::gpio::Output;
use embassy_time::{Delay, Instant};

use anova_oven_pico_core::lcd_plan::{plan_lcd, LcdAnimator};

use crate::display::{DisplayBackend, ViewSpec};

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

/// The degree sign in the HD44780's own character ROM. The driver's
/// `CharsetUniversal` has no mapping for `'°'` and its `EmptyFallback` would
/// silently render a space, so the one glyph the planner emits outside ASCII is
/// written as a raw byte (see [`LcdController::write_row`]).
const DEGREE_BYTE: u8 = 0xDF;

pub(crate) struct LcdController {
    lcd: LcdDriver,
    delay: Delay,
    animator: LcdAnimator,
}

impl LcdController {
    pub(crate) fn new(lcd: LcdDriver, delay: Delay) -> Self {
        Self {
            lcd,
            delay,
            animator: LcdAnimator::new(),
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

    /// Write one full row of cells, starting at column 0. `cells` arrives from
    /// the planner already cut and space-padded to the panel width, so this
    /// neither measures nor pads — it just puts down what it is given.
    async fn write_row(&mut self, row: u8, cells: &str) {
        self.lcd.set_cursor_xy((0, row), &mut self.delay).await.ok();
        for ch in cells.chars() {
            let _ = match ch {
                '°' => self.lcd.write_byte(DEGREE_BYTE, &mut self.delay).await,
                _ => self.lcd.write_char(ch, &mut self.delay).await,
            };
        }
    }
}

impl DisplayBackend for LcdController {
    async fn configure(&mut self) {
        self.configure_lcd().await;
    }

    /// `display_task` re-renders the same `ViewSpec` every animation tick, so
    /// the clock is the only input that moves between polls: it is what walks
    /// the marquee, rotates the bottom row, and ticks the cook timer.
    async fn render(&mut self, view: &ViewSpec) {
        let now = Instant::now();
        let plan = plan_lcd(view, view.timer_age_secs(now));
        let frame = self.animator.tick(&plan, now);

        if let Some(row0) = frame.row0 {
            self.write_row(0, &row0).await;
        }
        if let Some(row1) = frame.row1 {
            self.write_row(1, &row1).await;
        }
    }
}
