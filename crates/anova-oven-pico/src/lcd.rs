//! `LcdController` — the `ui-lcd` display backend: drives the 16x2 HD44780
//! character LCD over its 4-bit parallel bus.
//!
//! Layout, slot rotation and marquee timing all live in
//! [`anova_oven_pico_core::lcd_plan`], which is pure and host-tested — timing
//! included, since [`LcdAnimator::tick`] is handed the clock rather than
//! reading it. What is left here is the panel itself: move the cursor and
//! strobe the bytes for whichever cells a tick reports as changed.
//!
//! # Why the planner diffs individual cells
//!
//! This bus is slow, and not because of the panel. `hd44780-driver` holds `EN`
//! for `delay_ms(2)` per nibble, so every byte — character or command — costs
//! two of those plus a 100 µs settle:
//!
//! | | cost |
//! | --- | --- |
//! | one character, or one cursor move | ~4.1 ms |
//! | a full 16-cell row (cursor + 16 characters) | ~70 ms |
//! | both rows | ~139 ms |
//!
//! `display_task` renders every 50 ms, so repainting one whole row already
//! costs more than a tick. Hence [`LcdFrame`]: the planner sends only the runs
//! of cells that changed, which takes a ticking cook timer from ~70 ms to
//! ~20 ms and a steady screen to nothing. It is also why a cursor move and a
//! character costing the *same* is the interesting number — that is what
//! decides when a span is worth splitting, as [`CellSpan`] explains.
//!
//! The 2 ms pulse is the driver's own choice and is roughly 4000x the ~450 ns
//! the HD44780 datasheet asks for; shortening it is the bigger win available
//! here, but it needs the dependency changed and real hardware to verify.

use hd44780_driver::non_blocking::{Cursor, CursorBlink, Display, DisplayMode, HD44780};

use embassy_rp::gpio::Output;
use embassy_time::{Delay, Instant};

use anova_oven_pico_core::lcd_plan::{plan_lcd, CellSpan, LcdAnimator, LcdFrame};

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

    /// Write one run of cells: seek to its column, then put down its
    /// characters. The controller auto-increments the cursor, so the run needs
    /// the one seek however long it is — and DDRAM rows aren't contiguous, so a
    /// run never spills from one row into the other.
    async fn write_span(&mut self, row: u8, span: &CellSpan) {
        self.lcd
            .set_cursor_xy((span.col, row), &mut self.delay)
            .await
            .ok();
        for ch in span.text.chars() {
            let _ = match ch {
                '°' => self.lcd.write_byte(DEGREE_BYTE, &mut self.delay).await,
                _ => self.lcd.write_char(ch, &mut self.delay).await,
            };
        }
    }

    async fn write_frame(&mut self, frame: &LcdFrame) {
        for span in &frame.row0 {
            self.write_span(0, span).await;
        }
        for span in &frame.row1 {
            self.write_span(1, span).await;
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
        self.write_frame(&frame).await;
    }
}
