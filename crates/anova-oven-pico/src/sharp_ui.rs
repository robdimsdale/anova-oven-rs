//! `SharpScreen` — the `ui-sharp-basic` display backend: renders a `ViewSpec`
//! onto the 400x240 Sharp Memory Display via `embedded-graphics`. Selected as
//! `ActiveScreen` in `screen.rs` when `ui-sharp-basic` is enabled.
//!
//! Cadence: `display_task` calls [`SharpScreen::render`] every `ANIM_TICK_MS`
//! (50 ms). `render` draws into the framebuffer and asks the driver to flush;
//! the driver ([`Sharp::flush`](crate::sharp::Sharp::flush)) sends only the
//! lines that changed since the last flush, so a typical UI update (a few text
//! lines) is a sub-millisecond transfer rather than a ~48 ms full-frame push.
//! When nothing changed the driver reports it and we issue the cheap 2-byte
//! VCOM toggle instead, keeping VCOM alternating well above the panel's >= 1 Hz
//! requirement.
//!
//! Unlike the 16x2 HD44780 (`lcd.rs`), the panel has room to show the whole
//! status at once, so there's no scrolling/slot animation here.

use alloc::format;

use embassy_rp::gpio::Output;
use embassy_rp::peripherals::SPI0;
use embassy_rp::spi::{Blocking, Spi};
use embedded_graphics::{
    mono_font::{
        ascii::{FONT_10X20, FONT_9X15},
        MonoFont, MonoTextStyle,
    },
    pixelcolor::BinaryColor,
    prelude::*,
    text::{Baseline, Text},
};

use anova_oven_pico_core::fsm::ViewSpec;

use crate::api::celcius_to_fahrenheit;
use crate::sharp::Sharp;

type Panel = Sharp<Spi<'static, SPI0, Blocking>, Output<'static>>;

/// White background (`BinaryColor::Off`), black ink (`BinaryColor::On`) — see
/// the color mapping in `sharp.rs`.
const INK: BinaryColor = BinaryColor::On;
const PAPER: BinaryColor = BinaryColor::Off;

pub struct SharpScreen {
    panel: Panel,
}

impl SharpScreen {
    pub fn new(spi: Spi<'static, SPI0, Blocking>, cs: Output<'static>) -> Self {
        Self {
            panel: Sharp::new(spi, cs),
        }
    }

    /// Blank the panel to white once at startup.
    pub async fn configure(&mut self) {
        self.panel.clear_white();
        let _ = self.panel.flush();
    }

    /// Draw `view`, then flush the lines that changed; on a tick where nothing
    /// changed, toggle VCOM cheaply instead. Mirrors `LcdController::render`'s
    /// signature so `display_task` is backend-agnostic. Kept `async` for that
    /// contract even though the SPI writes are blocking (see `sharp.rs`).
    pub async fn render(&mut self, view: &ViewSpec) {
        draw(&mut self.panel, view);
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

/// Draw one text line, top-left anchored at `(x, y)`.
fn line<D: DrawTarget<Color = BinaryColor>>(t: &mut D, s: &str, x: i32, y: i32, font: &MonoFont) {
    let style = MonoTextStyle::new(font, INK);
    let _ = Text::with_baseline(s, Point::new(x, y), style, Baseline::Top).draw(t);
}

/// Render a `ViewSpec` into the framebuffer (no flush). Generic over the draw
/// target so it can be unit-tested/previewed off-target later.
fn draw<D: DrawTarget<Color = BinaryColor>>(t: &mut D, view: &ViewSpec) {
    let _ = t.clear(PAPER);

    const M: i32 = 12; // left margin
    match view {
        ViewSpec::WifiInit => line(t, "Connecting to Wi-Fi...", M, 100, &FONT_10X20),
        ViewSpec::DhcpInit => line(t, "Getting an IP address...", M, 100, &FONT_10X20),
        ViewSpec::Connecting => line(t, "Connecting...", M, 100, &FONT_10X20),
        ViewSpec::ServerOffline => line(t, "Server offline", M, 100, &FONT_10X20),
        ViewSpec::UpstreamStale { disconnected_secs } => {
            line(t, "Oven disconnected", M, 90, &FONT_10X20);
            line(t, &format!("{disconnected_secs}s ago"), M, 116, &FONT_9X15);
        }
        ViewSpec::StartingCook { recipe_title } => {
            line(t, "Starting cook", M, 90, &FONT_10X20);
            line(t, recipe_title, M, 116, &FONT_9X15);
        }
        ViewSpec::NextStagePrompt { recipe_title } => {
            line(t, "Next stage?", M, 90, &FONT_10X20);
            line(t, recipe_title, M, 116, &FONT_9X15);
        }
        ViewSpec::RecipeBrowser {
            count,
            index,
            title,
        } => {
            if *count == 0 {
                line(t, "No recipes", M, 100, &FONT_10X20);
            } else {
                line(t, &format!("Recipe {}/{}", index + 1, count), M, 24, &FONT_9X15);
                line(t, title, M, 60, &FONT_10X20);
            }
        }
        ViewSpec::StopConfirmation { status, cook } => {
            let title = cook
                .as_ref()
                .map(|c| c.display_name())
                .or_else(|| status.as_ref().map(|s| s.phase()))
                .unwrap_or("Active cook");
            line(t, title, M, 70, &FONT_10X20);
            line(t, "Stop cooking?", M, 110, &FONT_10X20);
        }
        ViewSpec::Recovery {
            reset_count,
            panic_count,
            message,
        } => {
            line(t, "Recovery", M, 20, &FONT_10X20);
            line(
                t,
                &format!("resets {reset_count}  panics {panic_count}"),
                M,
                52,
                &FONT_9X15,
            );
            if let Some(msg) = message {
                line(t, msg, M, 84, &FONT_9X15);
            }
        }
        ViewSpec::Status { status, cook } => draw_status(t, status.as_ref(), cook.as_ref()),
    }
}

fn draw_status<D: DrawTarget<Color = BinaryColor>>(
    t: &mut D,
    status: Option<&anova_oven_api::OvenStatus>,
    cook: Option<&anova_oven_api::CurrentCook>,
) {
    const M: i32 = 12;

    let Some(status) = status else {
        line(t, "Status: N/A", M, 100, &FONT_10X20);
        return;
    };

    // Title: cook name, else manual-cook indicator, else the oven mode.
    let title = if let Some(cook) = cook {
        cook.display_name()
    } else if status.is_cooking() {
        "Manual cook"
    } else {
        status.mode.as_str()
    };
    line(t, title, M, 12, &FONT_10X20);

    // Headline temperature: current -> target.
    let cur = celcius_to_fahrenheit(status.current_temperature_c());
    let mut temp = format!("{cur:.0}F");
    if let Some(target_c) = status.target_temperature_c {
        temp.push_str(&format!("  ->  {:.0}F", celcius_to_fahrenheit(target_c)));
    }
    line(t, &temp, M, 48, &FONT_10X20);

    // Detail rows.
    let mut y = 88;
    if let Some(remaining) = status.timer_remaining_secs() {
        let (h, m, s) = (remaining / 3600, (remaining % 3600) / 60, remaining % 60);
        let timer = if h > 0 {
            format!("Timer  {h}:{m:02}:{s:02}")
        } else {
            format!("Timer  {m:02}:{s:02}")
        };
        line(t, &timer, M, y, &FONT_9X15);
        y += 24;
    }
    if let Some(probe_c) = status.probe_temperature_c {
        line(
            t,
            &format!("Probe  {:.0}F", celcius_to_fahrenheit(probe_c)),
            M,
            y,
            &FONT_9X15,
        );
        y += 24;
    }
    if let Some(steam) = status.steam_target_pct {
        line(t, &format!("Steam  {steam:.0}%"), M, y, &FONT_9X15);
        y += 24;
    }
    line(t, &format!("Phase  {}", status.phase()), M, y, &FONT_9X15);
}
