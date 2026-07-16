//! Low-level driver for the Adafruit 2.7" Sharp Memory Display (LS027B7DH01,
//! 400x240), exposed as an `embedded-graphics` [`DrawTarget`]. `sharp_ui.rs`
//! wraps this into the `ViewSpec`-rendering `SharpScreen` display backend.
//!
//! Three panel quirks the protocol has to respect:
//!
//! 1. **Wire order is LSB-first.** The RP2040 SPI block only clocks MSB-first,
//!    so every byte we send is pre-reversed with [`u8::reverse_bits`].
//! 2. **CS is active HIGH** (unlike normal SPI). The caller passes a plain
//!    GPIO `OutputPin`; we drive it high for the duration of a transfer and
//!    leave it low (deselected) at rest — do NOT wire it to the hardware CS.
//! 3. **VCOM must alternate at >= 1 Hz** or the LCD accumulates a DC bias and
//!    the image sticks / degrades. Both [`flush`](Sharp::flush) and the cheap
//!    2-byte [`toggle_vcom`](Sharp::toggle_vcom) flip it; the backend calls one
//!    or the other every display tick (see `sharp_ui::SharpScreen::render`).
//!
//! Pixel data bit `1` = reflective white, `0` = black. We map
//! `BinaryColor::On` (the text foreground) to black on a white background.
//!
//! The SPI transfer is blocking (`embedded_hal::spi::SpiBus`). [`flush`](Sharp::flush)
//! sends only the lines that changed since the last successful flush — the panel
//! retains any line we don't re-address in its own memory — so a full ~12 KB /
//! ~50 ms frame is the worst case, not the norm: a few changed text lines
//! transfer (and bit-reverse) in well under a millisecond. Even a full flush
//! stays well within the 8 s watchdog and doesn't disturb cyw43's autonomous
//! PIO/DMA radio path. (Async/DMA would need a second DMA channel's `DMA_IRQ_0`
//! binding, which cyw43 already owns exclusively — see main.rs.)

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{Dimensions, OriginDimensions, Size},
    pixelcolor::BinaryColor,
    Pixel,
};
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::SpiBus;

pub const WIDTH: usize = 400;
pub const HEIGHT: usize = 240;

const BYTES_PER_LINE: usize = WIDTH / 8; // 50
const FRAME_LEN: usize = BYTES_PER_LINE * HEIGHT; // 12000

// Mode-byte flags, in the panel's native LSB-first bit semantics (reversed
// on the wire below).
const CMD_WRITE: u8 = 0x01;
const CMD_VCOM: u8 = 0x02;

/// One full transfer = mode byte + per-line (address + data + line-end 0x00)
/// + trailing frame-end 0x00.
const TX_LEN: usize = 1 + HEIGHT * (2 + BYTES_PER_LINE) + 1;

pub struct Sharp<SPI, CS> {
    spi: SPI,
    cs: CS,
    frame: [u8; FRAME_LEN],
    /// The framebuffer contents last successfully pushed to the panel. [`flush`]
    /// diffs `frame` against this per line and sends only the lines that differ;
    /// it is updated (for the sent lines) only after a successful write, so a
    /// failed transfer is retried in full on the next flush.
    last_sent: [u8; FRAME_LEN],
    vcom: bool,
    /// Forces the next [`flush`] to send every line regardless of the diff. Set
    /// at construction (the panel's power-on contents are undefined) and by
    /// [`clear_white`](Self::clear_white).
    force_full: bool,
}

impl<SPI: SpiBus, CS: OutputPin> Sharp<SPI, CS> {
    /// Wraps an SPI bus (mode 0, <= 2 MHz) and the active-high CS pin. Starts
    /// deselected with an all-white framebuffer and a forced full first flush.
    pub fn new(spi: SPI, mut cs: CS) -> Self {
        let _ = cs.set_low(); // deselected (active-high CS)
        Self {
            spi,
            cs,
            frame: [0xFF; FRAME_LEN],
            last_sent: [0xFF; FRAME_LEN],
            vcom: false,
            force_full: true,
        }
    }

    /// Reset the framebuffer to all-white and force the next [`flush`] to repaint
    /// the whole panel. Does not touch the panel until that flush.
    pub fn clear_white(&mut self) {
        self.frame = [0xFF; FRAME_LEN];
        self.force_full = true;
    }

    fn set_pixel(&mut self, x: usize, y: usize, black: bool) {
        if x >= WIDTH || y >= HEIGHT {
            return;
        }
        let idx = y * BYTES_PER_LINE + x / 8;
        let mask = 1u8 << (x % 8); // bit0 = leftmost column (LSB-first panel)
        if black {
            self.frame[idx] &= !mask;
        } else {
            self.frame[idx] |= mask;
        }
    }

    /// Push the lines that changed since the last successful flush and toggle
    /// VCOM. Returns `Ok(true)` if any line was sent (VCOM toggled as part of
    /// the write), or `Ok(false)` if nothing changed — in which case the caller
    /// should issue [`toggle_vcom`](Self::toggle_vcom) to keep VCOM alternating.
    ///
    /// Because the panel retains any line we don't re-address, sending only the
    /// dirty lines is exact, and cheaper in both SPI time and bit-reversal in
    /// proportion to how little changed. Whole rows are still the atomic unit:
    /// a line with even one changed pixel costs its full `BYTES_PER_LINE`.
    pub fn flush(&mut self) -> Result<bool, SPI::Error> {
        // Worst case (every line dirty) is exactly the original full-frame size.
        let mut tx = [0u8; TX_LEN];
        // Remember which lines we packed, so the shadow is updated only for
        // those, and only after the write succeeds.
        let mut dirty = [false; HEIGHT];

        // tx[0] holds the mode byte; fill it in once we know we're sending.
        let mut p = 1;
        let mut any = false;
        for (line, is_dirty) in dirty.iter_mut().enumerate() {
            let start = line * BYTES_PER_LINE;
            let end = start + BYTES_PER_LINE;
            if !self.force_full && self.frame[start..end] == self.last_sent[start..end] {
                continue;
            }
            *is_dirty = true;
            any = true;
            tx[p] = ((line + 1) as u8).reverse_bits(); // 1-indexed line address
            p += 1;
            for i in start..end {
                tx[p] = self.frame[i].reverse_bits();
                p += 1;
            }
            tx[p] = 0x00; // end of line
            p += 1;
        }

        if !any {
            return Ok(false);
        }

        let mode = CMD_WRITE | if self.vcom { CMD_VCOM } else { 0 };
        tx[0] = mode.reverse_bits();
        tx[p] = 0x00; // end of frame
        p += 1;

        let _ = self.cs.set_high();
        let r = self.spi.write(&tx[..p]);
        let _ = self.cs.set_low();
        r?;

        // Commit only after a successful write: advance VCOM, spend the forced
        // full repaint, and mark the sent lines as now matching the panel.
        self.vcom = !self.vcom;
        self.force_full = false;
        for (line, &is_dirty) in dirty.iter().enumerate() {
            if is_dirty {
                let start = line * BYTES_PER_LINE;
                let end = start + BYTES_PER_LINE;
                self.last_sent[start..end].copy_from_slice(&self.frame[start..end]);
            }
        }
        Ok(true)
    }

    /// Cheap VCOM maintenance: sends only the 2-byte no-op command (no line
    /// data), toggling VCOM. Use this on idle ticks to keep VCOM alternating
    /// without repainting the whole frame.
    pub fn toggle_vcom(&mut self) -> Result<(), SPI::Error> {
        let mode = if self.vcom { CMD_VCOM } else { 0 };
        self.vcom = !self.vcom;
        let tx = [mode.reverse_bits(), 0x00];

        let _ = self.cs.set_high();
        let r = self.spi.write(&tx);
        let _ = self.cs.set_low();
        r
    }
}

impl<SPI: SpiBus, CS: OutputPin> OriginDimensions for Sharp<SPI, CS> {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl<SPI: SpiBus, CS: OutputPin> DrawTarget for Sharp<SPI, CS> {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let bounds = self.bounding_box();
        for Pixel(coord, color) in pixels {
            if bounds.contains(coord) {
                // On = foreground (text) = black; Off = white.
                self.set_pixel(coord.x as usize, coord.y as usize, color == BinaryColor::On);
            }
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.frame = [if color == BinaryColor::On { 0x00 } else { 0xFF }; FRAME_LEN];
        Ok(())
    }
}
