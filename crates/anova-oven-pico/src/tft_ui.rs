//! `TftScreen` — the `ui-tft-basic` display backend (Layer B): renders a
//! `ViewSpec` onto the Adafruit 2.0" 320x240 colour IPS TFT. Selected as
//! `ActiveScreen` in `screen.rs` when `ui-tft-basic` is enabled.
//!
//! Layout is panel-independent and lives in `graphics_view` (Layer C), which
//! picks its middle font tier from the 320x240 size. This module owns the
//! TFT-specific policy, which is the usual draw-then-flush plus one thing the
//! monochrome panels have no use for: choosing the frame's colours.
//!
//! `tft.rs` keeps a 1-bit framebuffer and applies colour at flush time (see its
//! module docs for why a full RGB565 framebuffer can't fit in RP2040 SRAM), so
//! colour here is per *screen*, not per pixel. That is enough to carry the one
//! thing colour is genuinely good at on a status readout: telling the user at a
//! glance that something needs attention, without their having to read a word
//! of it.
//!
//! Cadence: `display_task` calls `render` every `ANIM_TICK_MS` (50 ms). A full
//! repaint is ~150 KB, about 38 ms at 32 MHz, and happens only when the whole
//! screen changes (including on a palette change); a typical text update
//! rewrites a few rows.

use embassy_rp::gpio::Output;
use embassy_rp::peripherals::SPI0;
use embassy_rp::spi::{Blocking, Spi};
use embassy_time::Delay;

use anova_oven_pico_core::fsm::ViewSpec;

use crate::display::DisplayBackend;
use crate::graphics_view::render_view;
use crate::tft::{rgb565, Palette, Tft};

const BLACK: u16 = rgb565(0, 0, 0);
const WHITE: u16 = rgb565(0xFF, 0xFF, 0xFF);
/// Attention without alarm — a warm amber reads clearly against black and is
/// distinguishable from white at a glance across a kitchen.
const AMBER: u16 = rgb565(0xFF, 0xB0, 0x20);
/// Reserved for the post-crash screen, which is the one view that means
/// something went wrong rather than something needs doing.
const RED: u16 = rgb565(0xFF, 0x3B, 0x30);

/// Colour the whole screen by what it's saying. Everything routine is white on
/// black; screens that want the user to *do* something are amber; the recovery
/// screen is red.
fn palette_for(view: &ViewSpec) -> Palette {
    let ink = match view {
        ViewSpec::Recovery { .. } => RED,
        ViewSpec::NextStagePrompt { .. }
        | ViewSpec::StopConfirmation { .. }
        | ViewSpec::ServerOffline
        | ViewSpec::UpstreamStale { .. } => AMBER,
        ViewSpec::WifiInit
        | ViewSpec::DhcpInit
        | ViewSpec::Connecting
        | ViewSpec::StartingCook { .. }
        | ViewSpec::RecipeBrowser { .. }
        | ViewSpec::Status { .. } => WHITE,
    };
    Palette { ink, paper: BLACK }
}

type Panel = Tft<Spi<'static, SPI0, Blocking>, Output<'static>, Output<'static>, Output<'static>>;

pub struct TftScreen {
    panel: Panel,
}

impl TftScreen {
    pub fn new(
        spi: Spi<'static, SPI0, Blocking>,
        dc: Output<'static>,
        cs: Output<'static>,
        rst: Output<'static>,
    ) -> Self {
        Self {
            panel: Tft::new(
                spi,
                dc,
                cs,
                rst,
                Palette {
                    ink: WHITE,
                    paper: BLACK,
                },
            ),
        }
    }
}

impl DisplayBackend for TftScreen {
    /// Reset, configure and blank the panel once at startup. The driver's reset
    /// and sleep-out waits are blocking (`embassy_time::Delay`): they add up to
    /// ~380 ms, spent in `main` before any other task is spawned and long
    /// before the watchdog starts, so there is nothing to yield to.
    async fn configure(&mut self) {
        if self.panel.init(&mut Delay).is_err() {
            // Nothing useful to retry: the bus is write-only, so there is no
            // feedback to act on, and an unconfigured panel stays dark. Log it
            // so an attached probe shows why the screen is blank.
            defmt::warn!("TFT init failed; panel will stay dark");
        }
    }

    /// Set the frame's colours, draw `view` via the shared layout, then flush
    /// the rows that changed. A failed transfer leaves the driver's shadow
    /// untouched, so the next tick retries it.
    async fn render(&mut self, view: &ViewSpec) {
        self.panel.set_palette(palette_for(view));
        render_view(&mut self.panel, view);
        let _ = self.panel.flush();
    }
}
