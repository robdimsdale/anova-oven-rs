//! `SharpScreen` — the `ui-sharp-basic` display backend (Layer B): renders a
//! `ViewSpec` onto the 400x240 Sharp Memory Display. Selected as `ActiveScreen`
//! in `screen.rs` when `ui-sharp-basic` is enabled.
//!
//! Layout is panel-independent and lives in `graphics_view` (Layer C); this
//! module owns only the Sharp-specific policy: draw the view into the driver's
//! framebuffer, then flush the changed lines — or, on a tick where nothing
//! changed, issue the cheap 2-byte VCOM toggle to keep VCOM alternating well
//! above the panel's >= 1 Hz requirement.
//!
//! Cadence: `display_task` calls `render` every `ANIM_TICK_MS` (50 ms). A full
//! frame flush is the worst case (~50 ms); the driver's dirty-line flush makes a
//! typical text update sub-millisecond (see `sharp.rs`).

use embassy_rp::gpio::Output;
use embassy_rp::peripherals::SPI0;
use embassy_rp::spi::{Blocking, Spi};

use anova_oven_pico_core::fsm::ViewSpec;

use crate::display::DisplayBackend;
use crate::graphics_view::render_view;
use crate::sharp::Sharp;

type Panel = Sharp<Spi<'static, SPI0, Blocking>, Output<'static>>;

pub struct SharpScreen {
    panel: Panel,
}

impl SharpScreen {
    pub fn new(spi: Spi<'static, SPI0, Blocking>, cs: Output<'static>) -> Self {
        Self {
            panel: Sharp::new(spi, cs),
        }
    }
}

impl DisplayBackend for SharpScreen {
    /// Blank the panel to white once at startup.
    async fn configure(&mut self) {
        self.panel.clear_white();
        let _ = self.panel.flush();
    }

    /// Draw `view` via the shared layout, then flush the lines that changed; on
    /// a tick where nothing changed, toggle VCOM cheaply instead.
    async fn render(&mut self, view: &ViewSpec) {
        render_view(&mut self.panel, view);
        match self.panel.flush() {
            // Changed lines were sent; the write toggled VCOM.
            Ok(true) => {}
            // Nothing changed — keep VCOM alternating with the 2-byte no-op.
            Ok(false) => {
                let _ = self.panel.toggle_vcom();
            }
            // Transfer failed; the shadow is untouched, so the next tick retries.
            Err(_) => {}
        }
    }
}
