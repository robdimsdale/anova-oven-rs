//! Low-level driver for the Adafruit 2.0" 320x240 colour IPS TFT (product
//! 4311, ST7789), exposed as an `embedded-graphics` [`DrawTarget`].
//! `tft_ui.rs` wraps this into the `ViewSpec`-rendering `TftScreen` backend.
//!
//! **Why a 1-bit framebuffer on a colour panel.** A full RGB565 framebuffer is
//! 320 x 240 x 2 = 150 KB, and a shadow copy to diff against would be 300 KB —
//! more than the RP2040's entire 264 KB of SRAM. So this driver keeps the same
//! *1-bit* framebuffer the other two panels use (9,600 bytes, plus a 9,600-byte
//! shadow — actually less than `sharp.rs` spends) and expands it to RGB565 one
//! scanline at a time as it flushes. That keeps the shared `graphics_view`
//! layout code, the `DrawTarget<Color = BinaryColor>` contract and the
//! dirty-region diffing all completely unchanged.
//!
//! The cost is that a frame carries one ink colour and one background colour
//! rather than per-pixel colour. That is not much of a loss for a text readout,
//! and it is not no-colour either: the palette is applied at flush time, so the
//! backend can recolour the whole screen per view — see `tft_ui.rs`, which
//! turns the recovery screen red and attention screens amber.
//!
//! **Framebuffer layout** is row-major, MSB-first: `frame[y * BYTES_PER_ROW +
//! x / 8]`, bit `7 - (x % 8)`. [`flush`](Tft::flush) diffs per row and rewrites
//! only contiguous runs of changed rows, each as a single addressed window, so
//! a typical text update moves a few KB rather than the full 150 KB.
//!
//! **Orientation.** The panel is natively 240x320 portrait; [`MADCTL_LANDSCAPE`]
//! rotates it to 320x240. Because the controller's native size *is* 240x320,
//! there are no row/column offsets to correct for (unlike the 240x240 ST7789
//! breakouts).
//!
//! **Wiring** — 4-wire SPI, plus RESET. The breakout has an onboard regulator
//! and level shifter, so it is happy driven at 3.3V from the Pico. `BL` is
//! pulled high on the board (backlight on) and can be left unconnected; see
//! `docs/pico-displays.md`. `MISO` and `SDCS` are for the microSD slot and are
//! not used here.

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{Dimensions, OriginDimensions, Size},
    pixelcolor::BinaryColor,
    Pixel,
};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::SpiBus;

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 240;

const BYTES_PER_ROW: usize = WIDTH / 8; // 40
const FRAME_LEN: usize = BYTES_PER_ROW * HEIGHT; // 9600
/// One expanded scanline in RGB565.
const ROW_BYTES: usize = WIDTH * 2; // 640

const CMD_SWRESET: u8 = 0x01;
const CMD_SLPOUT: u8 = 0x11;
const CMD_NORON: u8 = 0x13;
const CMD_INVON: u8 = 0x21;
const CMD_DISPON: u8 = 0x29;
const CMD_CASET: u8 = 0x2A;
const CMD_RASET: u8 = 0x2B;
const CMD_RAMWR: u8 = 0x2C;
const CMD_COLMOD: u8 = 0x3A;
const CMD_MADCTL: u8 = 0x36;

/// 16 bits per pixel (RGB565).
const COLMOD_16BIT: u8 = 0x55;

/// Memory access control for landscape: MY (row address order) | MV (row/column
/// exchange), with the RGB/BGR bit clear. Two knobs live here if the picture
/// comes out wrong: swap MY for MX (`0x60`) to rotate 180 degrees, and OR in
/// `0x08` if red and blue are transposed.
const MADCTL_LANDSCAPE: u8 = 0xA0;

/// An RGB565 pair: what a set bit and a clear bit in the framebuffer become on
/// the panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub ink: u16,
    pub paper: u16,
}

/// RGB565 from 8-bit components.
pub const fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3)
}

/// Reset pulse and post-reset settle. Generous: this runs once at boot, and a
/// long jumper wire slows the edge.
const RESET_PULSE_MS: u32 = 20;
/// The ST7789 needs time out of sleep before it will accept a frame.
const SLPOUT_SETTLE_MS: u32 = 150;

pub struct Tft<SPI, DC, CS, RST> {
    spi: SPI,
    dc: DC,
    cs: CS,
    rst: RST,
    frame: [u8; FRAME_LEN],
    /// The framebuffer contents last successfully pushed to the panel.
    /// [`flush`](Tft::flush) diffs `frame` against this per row and sends only
    /// the rows that differ; it is updated (for the sent rows) only after a
    /// successful write, so a failed transfer is retried next time.
    last_sent: [u8; FRAME_LEN],
    palette: Palette,
    /// Forces the next [`flush`](Tft::flush) to send every row regardless of the
    /// diff. Set at construction (GRAM powers up undefined) and whenever the
    /// palette changes, since the diff tracks *bits*, not the colours they
    /// currently stand for.
    force_full: bool,
}

impl<SPI, DC, CS, RST> Tft<SPI, DC, CS, RST>
where
    SPI: SpiBus,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    /// Wraps a write-only SPI bus (mode 0) plus the data/command, active-low
    /// chip-select and active-low reset pins. Does not touch the panel — call
    /// [`init`](Tft::init) before drawing.
    pub fn new(spi: SPI, mut dc: DC, mut cs: CS, rst: RST, palette: Palette) -> Self {
        let _ = dc.set_low();
        let _ = cs.set_high(); // deselected (active-low CS)
        Self {
            spi,
            dc,
            cs,
            rst,
            frame: [0x00; FRAME_LEN],
            last_sent: [0x00; FRAME_LEN],
            palette,
            force_full: true,
        }
    }

    /// Hardware-reset the controller, configure it, paint the whole panel in
    /// the background colour, then enable the display. Painting before
    /// `DISPON` matters: GRAM powers up with undefined contents, so enabling
    /// first would flash noise at the user.
    pub fn init<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), SPI::Error> {
        let _ = self.rst.set_high();
        delay.delay_ms(RESET_PULSE_MS);
        let _ = self.rst.set_low();
        delay.delay_ms(RESET_PULSE_MS);
        let _ = self.rst.set_high();
        delay.delay_ms(RESET_PULSE_MS);

        self.command(CMD_SWRESET, &[])?;
        delay.delay_ms(SLPOUT_SETTLE_MS);
        self.command(CMD_SLPOUT, &[])?;
        delay.delay_ms(SLPOUT_SETTLE_MS);
        self.command(CMD_COLMOD, &[COLMOD_16BIT])?;
        self.command(CMD_MADCTL, &[MADCTL_LANDSCAPE])?;
        // IPS panels of this family run inverted; without this the image is a
        // photographic negative.
        self.command(CMD_INVON, &[])?;
        self.command(CMD_NORON, &[])?;
        delay.delay_ms(RESET_PULSE_MS);

        self.clear_frame();
        self.flush()?;
        self.command(CMD_DISPON, &[])
    }

    /// Reset the framebuffer to all-background and force a full repaint on the
    /// next [`flush`](Tft::flush). Does not touch the panel until that flush.
    pub fn clear_frame(&mut self) {
        self.frame = [0x00; FRAME_LEN];
        self.force_full = true;
    }

    /// Recolour the whole screen. The framebuffer diff tracks bits rather than
    /// colours, so a changed palette has to force a full repaint — which is why
    /// an unchanged palette is a no-op rather than an unconditional assignment.
    pub fn set_palette(&mut self, palette: Palette) {
        if palette != self.palette {
            self.palette = palette;
            self.force_full = true;
        }
    }

    /// Push the rows that changed since the last successful flush, coalescing
    /// adjacent changed rows into one addressed window. Returns `Ok(true)` if
    /// anything was sent — there is no VCOM upkeep here, so an unchanged frame
    /// costs no bus traffic at all.
    pub fn flush(&mut self) -> Result<bool, SPI::Error> {
        let mut any = false;
        let mut y = 0;
        while y < HEIGHT {
            if !self.row_dirty(y) {
                y += 1;
                continue;
            }
            let start = y;
            while y < HEIGHT && self.row_dirty(y) {
                y += 1;
            }
            self.write_rows(start, y)?;
            // Commit the shadow only for rows that actually landed; an error
            // above returns early and leaves them stale so the next flush
            // retries them.
            let (from, to) = (start * BYTES_PER_ROW, y * BYTES_PER_ROW);
            self.last_sent[from..to].copy_from_slice(&self.frame[from..to]);
            any = true;
        }
        self.force_full = false;
        Ok(any)
    }

    fn row_dirty(&self, y: usize) -> bool {
        if self.force_full {
            return true;
        }
        let from = y * BYTES_PER_ROW;
        let to = from + BYTES_PER_ROW;
        self.frame[from..to] != self.last_sent[from..to]
    }

    /// Address rows `y0..y1` as one window, then stream them expanded to
    /// RGB565. GRAM auto-increments across the window, so the whole run is a
    /// single `RAMWR` with no re-addressing between rows.
    fn write_rows(&mut self, y0: usize, y1: usize) -> Result<(), SPI::Error> {
        let (x_end, y_start, y_last) = (WIDTH as u16 - 1, y0 as u16, y1 as u16 - 1);
        self.command(CMD_CASET, &[0, 0, (x_end >> 8) as u8, x_end as u8])?;
        self.command(
            CMD_RASET,
            &[
                (y_start >> 8) as u8,
                y_start as u8,
                (y_last >> 8) as u8,
                y_last as u8,
            ],
        )?;
        self.command(CMD_RAMWR, &[])?;

        // One scanline of expanded pixels. 640 bytes of stack, against the
        // ~12 KB `sharp.rs` puts there for the same job.
        let mut row_buf = [0u8; ROW_BYTES];
        let _ = self.cs.set_low();
        let _ = self.dc.set_high(); // data
        let mut result = Ok(());
        for y in y0..y1 {
            let row = &self.frame[y * BYTES_PER_ROW..(y + 1) * BYTES_PER_ROW];
            for (x, out) in row_buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                let lit = row[x / 8] & (0x80 >> (x % 8)) != 0;
                let c = if lit {
                    self.palette.ink
                } else {
                    self.palette.paper
                };
                out[0] = (c >> 8) as u8;
                out[1] = c as u8;
            }
            result = self.spi.write(&row_buf);
            if result.is_err() {
                break;
            }
        }
        let _ = self.cs.set_high();
        result
    }

    /// Send one command byte (D/C low) followed by its arguments (D/C high).
    fn command(&mut self, cmd: u8, args: &[u8]) -> Result<(), SPI::Error> {
        let _ = self.cs.set_low();
        let _ = self.dc.set_low();
        let mut r = self.spi.write(&[cmd]);
        if r.is_ok() && !args.is_empty() {
            let _ = self.dc.set_high();
            r = self.spi.write(args);
        }
        let _ = self.cs.set_high();
        r
    }

    fn set_pixel(&mut self, x: usize, y: usize, lit: bool) {
        if x >= WIDTH || y >= HEIGHT {
            return;
        }
        let idx = y * BYTES_PER_ROW + x / 8;
        let mask = 0x80u8 >> (x % 8); // bit7 = leftmost column
        if lit {
            self.frame[idx] |= mask;
        } else {
            self.frame[idx] &= !mask;
        }
    }
}

impl<SPI, DC, CS, RST> OriginDimensions for Tft<SPI, DC, CS, RST> {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl<SPI, DC, CS, RST> DrawTarget for Tft<SPI, DC, CS, RST>
where
    SPI: SpiBus,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let bounds = self.bounding_box();
        for Pixel(coord, color) in pixels {
            if bounds.contains(coord) {
                // On = foreground (text) = ink; Off = background.
                self.set_pixel(coord.x as usize, coord.y as usize, color == BinaryColor::On);
            }
        }
        Ok(())
    }

    fn clear(&mut self, color: Self::Color) -> Result<(), Self::Error> {
        self.frame = [if color == BinaryColor::On { 0xFF } else { 0x00 }; FRAME_LEN];
        Ok(())
    }
}
