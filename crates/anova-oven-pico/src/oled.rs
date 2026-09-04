//! Low-level driver for the Adafruit 2.42" 128x64 monochrome OLED module
//! (product 2719), exposed as an `embedded-graphics` [`DrawTarget`].
//! `oled_ui.rs` wraps this into the `ViewSpec`-rendering `OledScreen` display
//! backend.
//!
//! **Two controllers, one board.** Adafruit revised 2719 in September 2023:
//! boards made before then carry an SSD1305, later ones an SSD1309. The pinout,
//! the wire protocol and the GDDRAM layout are identical; only the power-on
//! configuration differs, so the two are one driver with two init tables
//! ([`Variant`]). Picking the wrong one is the likeliest cause of a panel that
//! stays dark — `oled_ui.rs` selects it from a Cargo feature so it's a one-word
//! change.
//!
//! **Wiring** — 4-wire SPI, the module's default mode (BS1=BS2=0; the mode is
//! set by solder jumpers on the back, see `docs/pico-displays.md`). The module
//! is 3.3V logic *and* 3.3V power, same as the Pico, so the HC4050 level
//! shifter in the kit is not used here — it exists for 5V hosts like an Arduino.
//!
//! **Bus speed.** The SSD1305's serial interface is specified to ~4 MHz;
//! reports of blank panels at Adafruit's 8 MHz library default are common, so
//! `main.rs` clocks SPI0 at 4 MHz. Even so a full 1 KB frame is ~2 ms, which is
//! why this driver — unlike `sharp.rs`, whose full frame costs ~50 ms — is
//! blocking with plenty of margin under the 8 s watchdog. (Async/DMA would need
//! a second DMA channel's `DMA_IRQ_0` binding, which cyw43 already owns.)
//!
//! **Memory layout.** GDDRAM is page-major: one page is an 8-pixel-tall,
//! 128-byte-wide band, and bit `n` of a byte is the pixel `n` rows down inside
//! that band. [`flush`](Oled::flush) diffs per page and rewrites only the pages
//! that changed, so a typical text update costs one or two 128-byte pages.
//!
//! Pixel bit `1` = lit. The renderer's `BinaryColor::On` (its ink) maps to lit,
//! giving white text on an unlit background — which is also the polarity that
//! keeps an OLED's cumulative on-time, and so its dimming, lowest.

use embedded_graphics::{
    draw_target::DrawTarget,
    geometry::{Dimensions, OriginDimensions, Size},
    pixelcolor::BinaryColor,
    Pixel,
};
use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use embedded_hal::spi::SpiBus;

pub const WIDTH: usize = 128;
pub const HEIGHT: usize = 64;

const PAGES: usize = HEIGHT / 8; // 8
const PAGE_LEN: usize = WIDTH; // 128 bytes per page
const FRAME_LEN: usize = PAGES * PAGE_LEN; // 1024

const CMD_DISPLAY_ON: u8 = 0xAF;
const CMD_DISPLAY_OFF: u8 = 0xAE;
const CMD_SET_CONTRAST: u8 = 0x81;
/// `0xB0 | page` selects the page a subsequent data write lands in.
const CMD_SET_PAGE_START: u8 = 0xB0;

/// Which controller the board carries. See the module docs: same board, same
/// protocol, different power-on configuration.
///
/// `dead_code` is allowed because a build constructs exactly one of these —
/// `oled_ui.rs` picks it from a Cargo feature — so the other is always unused
/// by construction, not by oversight.
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    /// Adafruit 2719 manufactured before September 2023.
    Ssd1305,
    /// Adafruit 2719 manufactured from September 2023 on.
    Ssd1309,
}

impl Variant {
    fn init_sequence(self) -> &'static [u8] {
        match self {
            Variant::Ssd1305 => &INIT_SSD1305,
            Variant::Ssd1309 => &INIT_SSD1309,
        }
    }

    /// The contrast byte each init table already sends (0x32 for the
    /// SSD1305, 0x6F for the SSD1309 — the two vendor tables pick different
    /// "normal" operating points). [`Oled::set_contrast`] restores this on a
    /// transition back to full brightness.
    pub fn normal_contrast(self) -> u8 {
        match self {
            Variant::Ssd1305 => 0x32,
            Variant::Ssd1309 => 0x6F,
        }
    }
}

/// SSD1305 power-on configuration, transcribed from Adafruit's own
/// `Adafruit_SSD1305` library (its `init_128x64` table), which is the sequence
/// the 2719 was validated against. The magic values are the vendor's; the
/// comments name what each one sets.
const INIT_SSD1305: [u8; 31] = [
    CMD_DISPLAY_OFF,
    0x04, // set lower column start = 4  (Adafruit's table; page writes re-set it)
    0x14, // set higher column start = 4
    0x40, // display start line = 0
    0x2E, // deactivate scroll
    CMD_SET_CONTRAST,
    0x32,
    0x82, // set brightness (SSD1305-only command)
    0x80,
    0xA1, // segment remap: column 127 -> SEG0
    0xA6, // normal (non-inverted) display
    0xA8, // multiplex ratio...
    0x3F, // ...1/64 duty
    0xAD, // master configuration...
    0x8E, // ...external VCC (the module boosts 3.3V on-board)
    0xC8, // COM scan direction: reversed
    0xD3, // display offset...
    0x40,
    0xD5, // display clock divide / oscillator frequency...
    0xF0,
    0xD8, // area colour mode: monochrome, normal power
    0x05,
    0xD9, // pre-charge period...
    0xF1,
    0xDA, // COM pins hardware configuration...
    0x12, // ...alternative, no left/right remap
    0x91, // greyscale lookup table (SSD1305-only)...
    0x3F,
    0x3F,
    0x3F,
    0x3F,
];

/// SSD1309 power-on configuration, from u8g2's `u8x8_d_ssd1309` 128x64 table.
/// The SSD1305-only commands (brightness, area colour, greyscale LUT, master
/// configuration) are absent — sending them to an SSD1309 is what leaves the
/// newer boards dark.
const INIT_SSD1309: [u8; 20] = [
    CMD_DISPLAY_OFF,
    0xD5, // display clock divide / oscillator frequency...
    0xA0,
    0xA8, // multiplex ratio...
    0x3F, // ...1/64 duty
    0x40, // display start line = 0
    0xD3, // display offset...
    0x00,
    0xA1, // segment remap: column 127 -> SEG0
    0xC8, // COM scan direction: reversed
    0xDA, // COM pins hardware configuration...
    0x12, // ...alternative, no left/right remap
    CMD_SET_CONTRAST,
    0x6F,
    0xD9, // pre-charge period...
    0xD3,
    0xDB, // VCOMH deselect level...
    0x20,
    0x2E, // deactivate scroll
    0xA4, // resume from RAM (not all-pixels-on)
];

/// Memory addressing mode (`0x20`) set to page addressing. Both variants power
/// up this way; sending it explicitly after either init table means neither
/// table has to be trusted on the point that [`flush`](Oled::flush) depends on.
const MEMORY_MODE_PAGE: [u8; 2] = [0x20, 0x02];

/// Reset pulse. The datasheet asks for microseconds; milliseconds cost nothing
/// once at boot and tolerate a slow rise on a long jumper wire.
const RESET_PULSE_MS: u32 = 10;
/// Adafruit's library waits 100 ms after the init table before enabling the
/// panel; the charge pump needs the settling time.
const POST_INIT_MS: u32 = 100;

pub struct Oled<SPI, DC, CS, RST> {
    spi: SPI,
    dc: DC,
    cs: CS,
    rst: RST,
    variant: Variant,
    frame: [u8; FRAME_LEN],
    /// The framebuffer contents last successfully pushed to the panel.
    /// [`flush`](Oled::flush) diffs `frame` against this per page and sends only
    /// the pages that differ; it is updated (for the sent pages) only after a
    /// successful write, so a failed transfer is retried in full next time.
    last_sent: [u8; FRAME_LEN],
    /// Forces the next [`flush`](Oled::flush) to send every page regardless of
    /// the diff. Set until [`init`](Oled::init) has cleared GDDRAM, whose
    /// power-on contents are undefined.
    force_full: bool,
}

impl<SPI, DC, CS, RST> Oled<SPI, DC, CS, RST>
where
    SPI: SpiBus,
    DC: OutputPin,
    CS: OutputPin,
    RST: OutputPin,
{
    /// Wraps a write-only SPI bus (mode 0, <= 4 MHz) plus the data/command,
    /// active-low chip-select, and active-low reset pins. Does not touch the
    /// panel — call [`init`](Oled::init) before drawing.
    pub fn new(spi: SPI, mut dc: DC, mut cs: CS, rst: RST, variant: Variant) -> Self {
        let _ = dc.set_low();
        let _ = cs.set_high(); // deselected (active-low CS)
        Self {
            spi,
            dc,
            cs,
            rst,
            variant,
            frame: [0x00; FRAME_LEN],
            last_sent: [0x00; FRAME_LEN],
            force_full: true,
        }
    }

    /// Hardware-reset the controller, apply the variant's configuration, blank
    /// GDDRAM, then turn the panel on. Blanking before enabling the display
    /// matters: the controller's RAM powers up with undefined contents, so
    /// enabling first would flash noise at the user.
    pub fn init<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), SPI::Error> {
        let _ = self.rst.set_high();
        delay.delay_ms(RESET_PULSE_MS);
        let _ = self.rst.set_low();
        delay.delay_ms(RESET_PULSE_MS);
        let _ = self.rst.set_high();
        delay.delay_ms(RESET_PULSE_MS);

        let init_sequence = self.variant.init_sequence();
        self.commands(init_sequence)?;
        self.commands(&MEMORY_MODE_PAGE)?;
        delay.delay_ms(POST_INIT_MS);

        self.clear_frame();
        self.flush()?;
        self.commands(&[CMD_DISPLAY_ON])
    }

    /// Set the SSD1305/1309's contrast register (segment drive current).
    /// Lower values slow — but do not stop — the organic material's aging,
    /// since that scales with cumulative current rather than being a
    /// threshold effect. See [`display_off`](Oled::display_off) for the only
    /// setting that actually halts it.
    pub fn set_contrast(&mut self, value: u8) -> Result<(), SPI::Error> {
        self.commands(&[CMD_SET_CONTRAST, value])
    }

    /// Blank the panel (`0xAE`) without touching GDDRAM: this cuts pixel
    /// current to zero, which is what actually stops aging rather than just
    /// slowing it. The last frame reappears untouched on
    /// [`display_on`](Oled::display_on).
    pub fn display_off(&mut self) -> Result<(), SPI::Error> {
        self.commands(&[CMD_DISPLAY_OFF])
    }

    /// Re-enable the panel (`0xAF`) after [`display_off`](Oled::display_off).
    pub fn display_on(&mut self) -> Result<(), SPI::Error> {
        self.commands(&[CMD_DISPLAY_ON])
    }

    /// Reset the framebuffer to all-unlit and force the next
    /// [`flush`](Oled::flush) to repaint every page. Does not touch the panel
    /// until that flush.
    pub fn clear_frame(&mut self) {
        self.frame = [0x00; FRAME_LEN];
        self.force_full = true;
    }

    /// Push the pages that changed since the last successful flush. Returns
    /// `Ok(true)` if any page was sent, `Ok(false)` if the frame was already on
    /// the panel — unlike the Sharp there is no VCOM upkeep, so an unchanged
    /// frame means genuinely no bus traffic.
    pub fn flush(&mut self) -> Result<bool, SPI::Error> {
        let mut any = false;
        for page in 0..PAGES {
            let start = page * PAGE_LEN;
            let end = start + PAGE_LEN;
            if !self.force_full && self.frame[start..end] == self.last_sent[start..end] {
                continue;
            }
            // Address the page and rewind the column pointer to 0. The column
            // start is split across two commands: high nibble (0x10 | hi) and
            // low nibble (0x00 | lo).
            self.write_page(page, start, end)?;
            self.last_sent[start..end].copy_from_slice(&self.frame[start..end]);
            any = true;
        }
        // Only spend the forced full repaint once every page has actually
        // landed; an error above returns early and leaves it set.
        self.force_full = false;
        Ok(any)
    }

    /// One page: the three addressing commands, then its 128 data bytes.
    /// Split out so the borrow of `self.frame` for the data write doesn't
    /// collide with the `&mut self` methods above it.
    fn write_page(&mut self, page: usize, start: usize, end: usize) -> Result<(), SPI::Error> {
        let cmds = [CMD_SET_PAGE_START | page as u8, 0x10, 0x00];
        self.commands(&cmds)?;

        // Destructured so the `&self.frame` data borrow is disjoint from the
        // `&mut` borrows of the bus and pins.
        let Self {
            spi, dc, cs, frame, ..
        } = self;
        let _ = cs.set_low();
        let _ = dc.set_high(); // data
        let r = spi.write(&frame[start..end]);
        let _ = cs.set_high();
        r
    }

    /// Send `bytes` with D/C held low, i.e. as commands.
    fn commands(&mut self, bytes: &[u8]) -> Result<(), SPI::Error> {
        let _ = self.cs.set_low();
        let _ = self.dc.set_low();
        let r = self.spi.write(bytes);
        let _ = self.cs.set_high();
        r
    }

    fn set_pixel(&mut self, x: usize, y: usize, lit: bool) {
        if x >= WIDTH || y >= HEIGHT {
            return;
        }
        let idx = (y / 8) * PAGE_LEN + x;
        let mask = 1u8 << (y % 8); // bit0 = topmost row of the page
        if lit {
            self.frame[idx] |= mask;
        } else {
            self.frame[idx] &= !mask;
        }
    }
}

impl<SPI, DC, CS, RST> OriginDimensions for Oled<SPI, DC, CS, RST> {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl<SPI, DC, CS, RST> DrawTarget for Oled<SPI, DC, CS, RST>
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
                // On = foreground (text) = lit; Off = dark.
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
