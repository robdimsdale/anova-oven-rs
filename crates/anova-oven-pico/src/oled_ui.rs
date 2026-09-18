//! `OledScreen` — the `ui-oled-basic` display backend (Layer B): renders a
//! `ViewSpec` onto the Adafruit 2.42" 128x64 OLED. Selected as `ActiveScreen`
//! in `screen.rs` when `ui-oled-basic` is enabled.
//!
//! Layout is panel-independent and lives in `graphics_view` (Layer C), which
//! picks its compact font tier from the 128x64 size and drops the detail rows
//! that don't fit. This module owns only the OLED-specific policy, which is
//! about as thin as a backend gets: draw the view into the driver's
//! framebuffer, then flush the pages that changed. An emissive panel has no
//! VCOM to maintain, so a tick where nothing changed costs no bus traffic at
//! all (contrast `sharp_ui.rs`).
//!
//! Cadence: `display_task` calls `render` every `ANIM_TICK_MS` (50 ms). A full
//! 1 KB frame at 4 MHz is ~2 ms; a typical text update touches one or two
//! 128-byte pages (see `oled.rs`).

use embassy_rp::gpio::Output;
use embassy_rp::peripherals::SPI0;
use embassy_rp::spi::{Blocking, Spi};
use embassy_time::{Delay, Duration, Instant};

use anova_oven_pico_core::fsm::{BacklightPolicy, ViewSpec};

use crate::display::DisplayBackend;
use crate::graphics_view::render_view;
use crate::oled::{Oled, Variant};

/// Contrast used while `BacklightPolicy::Dim` is active — low but not zero,
/// so the panel is still legible up close. Dimming only slows OLED aging
/// (it scales with cumulative current, not a threshold), so it is paired
/// with `OFF_AFTER_DIM` below rather than relied on alone.
const DIM_CONTRAST: u8 = 0x01;

/// How long to stay dimmed before cutting the panel fully off. This is
/// materially longer than the FSM's dim delay (currently 5s — see
/// `AppState::idle_dim_delay`) so a glance at a dim screen doesn't go dark
/// mid-read; tune on real hardware once burn-in is actually observed.
const OFF_AFTER_DIM: Duration = Duration::from_secs(300);

/// Which controller the wired board carries. Adafruit revised product 2719 from
/// SSD1305 to SSD1309 in September 2023 without changing the pinout or the
/// protocol, so the board can't be told apart over a write-only bus — build
/// with `--features ui-oled-basic,oled-ssd1309` for a board bought since then.
/// A panel that resets but stays dark is the symptom of guessing wrong.
#[cfg(not(feature = "oled-ssd1309"))]
const VARIANT: Variant = Variant::Ssd1305;
#[cfg(feature = "oled-ssd1309")]
const VARIANT: Variant = Variant::Ssd1309;

type Panel = Oled<Spi<'static, SPI0, Blocking>, Output<'static>, Output<'static>, Output<'static>>;

pub struct OledScreen {
    panel: Panel,
    /// Set when `BacklightPolicy::Dim` starts; cleared on `Full`. `render`
    /// checks it each tick and cuts the panel off once `OFF_AFTER_DIM`
    /// elapses without a `Full` bringing it back.
    dim_since: Option<Instant>,
    panel_off: bool,
}

impl OledScreen {
    pub fn new(
        spi: Spi<'static, SPI0, Blocking>,
        dc: Output<'static>,
        cs: Output<'static>,
        rst: Output<'static>,
    ) -> Self {
        Self {
            panel: Oled::new(spi, dc, cs, rst, VARIANT),
            dim_since: None,
            panel_off: false,
        }
    }
}

impl DisplayBackend for OledScreen {
    /// Reset, configure and blank the panel once at startup. The driver's
    /// reset and post-init waits are blocking (`embassy_time::Delay`): they add
    /// up to ~130 ms, spent in `main` before any other task is spawned and long
    /// before the watchdog starts, so there is nothing to yield to.
    async fn configure(&mut self) {
        if self.panel.init(&mut Delay).is_err() {
            // Nothing useful to retry: the bus is write-only, so there is no
            // feedback to act on, and an unconfigured panel stays dark. Log it
            // so an attached probe shows why the screen is blank.
            defmt::warn!("OLED init failed; panel will stay dark");
        }
    }

    /// Draw `view` via the shared layout, then flush the pages that changed.
    /// A failed transfer leaves the driver's shadow untouched, so the next tick
    /// retries it. Keeps drawing and flushing even while `panel_off` is set —
    /// GDDRAM writes work regardless of `0xAE`, so the frame is already
    /// correct and reappears with no extra work the moment the panel comes
    /// back on.
    async fn render(&mut self, view: &ViewSpec) {
        if let Some(since) = self.dim_since {
            if !self.panel_off && since.elapsed() >= OFF_AFTER_DIM {
                let _ = self.panel.display_off();
                self.panel_off = true;
            }
        }

        render_view(&mut self.panel, view);
        let _ = self.panel.flush();
    }

    /// `Dim` lowers contrast and starts the off-timer; `Full` (including
    /// `FullThenDimAfter`, whose entry intent is full — see
    /// `backlight::BacklightController::apply`) restores normal contrast,
    /// cancels the timer, and wakes the panel back up if it had gone dark.
    async fn set_backlight(&mut self, policy: BacklightPolicy) {
        match policy {
            BacklightPolicy::Dim => {
                if self.dim_since.is_none() {
                    self.dim_since = Some(Instant::now());
                }
                let _ = self.panel.set_contrast(DIM_CONTRAST);
            }
            BacklightPolicy::Full | BacklightPolicy::FullThenDimAfter(_) => {
                self.dim_since = None;
                if self.panel_off {
                    let _ = self.panel.display_on();
                    self.panel_off = false;
                }
                let _ = self.panel.set_contrast(VARIANT.normal_contrast());
            }
        }
    }
}
