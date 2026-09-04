# Pico display backends

The firmware supports four panels plus a headless build. Exactly one is
compiled in, chosen by a Cargo feature, because they all claim pins from the
same GP16–GP21 block and each needs different peripheral init in `main.rs`.

| Build | Panel | Bus |
| --- | --- | --- |
| `cargo run --release` | none (headless, defmt logging only) | — |
| `--features ui-lcd` | 16×2 HD44780 character LCD | 4-bit parallel |
| `--features ui-sharp-basic` | Adafruit 2.7" Sharp Memory Display (LS027B7DH01, 400×240) | SPI0 |
| `--features ui-oled-basic` | Adafruit 2.42" OLED (product 2719, 128×64) | SPI0 |
| `--features ui-tft-basic` | Adafruit 2.0" colour IPS TFT (product 4311, 320×240) | SPI0 |

`screen.rs` resolves the feature to `ActiveScreen` and const-asserts that at
most one is enabled; enabling two fails the build with that message.

## Code layers

The three graphical panels share everything above the wire:

```
ViewSpec                        (fsm.rs — what the FSM wants shown)
  └── view_plan::plan_view      (pico-core, host-tested: which lines, what
      │                          font role, centred or top-stacked)
      └── graphics_view         (fonts per panel-size tier, wrap measurement,
          │                      drop lines that don't fit, draw)
          ├── sharp.rs   → Sharp   (DrawTarget, dirty-line flush, VCOM upkeep)
          ├── oled.rs    → Oled    (DrawTarget, dirty-page flush)
          └── tft.rs     → Tft     (DrawTarget, dirty-row flush, 1-bit → RGB565)
```

Adding another panel means a driver implementing `DrawTarget<Color =
BinaryColor>`, a thin `*_ui.rs` backend implementing `DisplayBackend`, a
`ui-*` feature that pulls in `_graphics`, and one arm in `screen.rs`. The
layout code should not need to change beyond possibly a font tier:
`graphics_view` picks its tier from the panel's reported size, and trims
trailing lines that don't fit.

Every graphical driver here is a **1-bit** `DrawTarget`, including the colour
TFT. That is not an oversight — see the TFT section below for why, and for what
colour the panel does get.

### Font tiers

Tiers are bounded by **width**, because the hero temperature is the one line
the planner never wraps, and a hero font too wide for the content box gets the
temperature clipped:

| Tier | Panel | Hero font | Worst-case hero (`888F -> 888F`) vs. content width |
| --- | --- | --- | --- |
| compact | 128×64 OLED | 9x18B | 108 px vs. 120 px |
| middle | 320×240 TFT | fub30 | 264 px vs. 300 px |
| large | 400×240 Sharp | fub35 | 320 px vs. 380 px |

Note that the TFT and the Sharp are the same height but not the same tier:
`fub35` needs 320 px for a hero the 320-wide panel has only 300 px for.

## 2.42" OLED (Adafruit 2719)

### Controller variant — read this first

Adafruit revised the board in **September 2023**: earlier units carry an
**SSD1305**, later ones an **SSD1309**. The pinout, the wire protocol and the
GDDRAM layout are identical, but the two controllers accept different
initialisation, and the bus is write-only so the firmware cannot tell them
apart. Build for the newer board with:

```
cargo run --release --features ui-oled-basic,oled-ssd1309
```

A panel that stays completely dark, with wiring that checks out, almost always
means the wrong variant. Both init tables live in `src/oled.rs`.

### Interface mode

The module speaks 8-bit parallel, I²C, or 4-wire SPI, selected by **solder
jumpers on the back** (BS1/BS2). The firmware uses **SPI**: fastest, fewest
wires, and it reuses the SPI0 setup the Sharp backend already needed. SPI mode
is `BS1=0, BS2=0` — on the newer 2.4"-style board that means resistors fitted
at **R5 and R3**, with **R4 and R2** empty. Boards often ship in 8-bit mode, so
check before wiring. (Adafruit's guide has photos for each board revision:
<https://learn.adafruit.com/1-5-and-2-4-monochrome-128x64-oled-display-module/assembly>)

I²C would also work electrically but not well here: a full 1 KB frame is ~2 ms
over SPI at 4 MHz versus ~25 ms at 400 kHz, against a 50 ms render tick.

### Wiring

The module's 20-pin header is unmarked; **pin 1 is the leftmost**, counting up
to 20 on the right.

| Module pin | Label | Pico W |
| --- | --- | --- |
| 1 | GND | GND |
| 2 | 3V | 3V3 (pin 36) |
| 3 | — | leave unconnected |
| 4 | D/C | GP16 |
| 7 | D0 (SPI clock) | GP18 (SPI0 SCK) |
| 8 | D1 (SPI data in) | GP19 (SPI0 TX) |
| 15 | CS | GP17 |
| 16 | RESET | GP20 |
| 20 | frame ground | GND or floating |

Pins 5–6 (WR/RD) and 9–14 (D2–D7) are 8-bit-mode only: leave them
unconnected. Pins 17–19 are not connected.

Notes:

- **No level shifter.** The module is 3.3V logic *and* 3.3V power, the same as
  the Pico. The HC4050 (CD4050BE) in the kit is there for 5V hosts such as an
  Arduino; wiring it in here would only add propagation delay and a part to
  get wrong.
- **The 220 µF capacitor is worth fitting** across 3V3 and GND close to the
  module. The panel draws ~50 mA average from 3.3V and considerably more in
  bursts as the charge pump runs, and the Pico W's regulator is also feeding
  the CYW43 radio. Bulk capacitance there costs nothing and heads off brownout
  resets that would look like random watchdog reboots.
- CS is **active low** here — the opposite of the Sharp panel, whose CS is
  active high.

### Layout on 128×64

`graphics_view`'s compact font tier is sized for this panel: a 9x18B hero
(the widest string the planner emits, `888F -> 888F`, is 108 px against a
120 px content width), a 12 px title, and 9 px detail rows. That budget fits a
title, the hero temperature and three detail rows. A cooking status with
timer, probe, steam *and* phase rows needs one more, so the phase row is
dropped — `fit_line_count` trims from the end, and the planner orders a top-stacked
plan by importance, so what goes is what mattered least.

### Burn-in mitigation

An OLED ages where it has been lit longest, and this firmware shows a mostly
static status screen, so `state.rs` routes `BacklightPolicy` to `OledScreen`
(`display::BacklightNotifier`, a second signal alongside `DisplayNotifier` —
`display_task` selects on both) in addition to the character LCD's
`ctx.backlight`, which has no equivalent hardware here (no `BL` pin — see the
top of this section).

Two tiers, because contrast alone doesn't stop the aging, only slows it —
aging tracks cumulative current, not a brightness threshold:

- **`Dim`** (the FSM's 5 s idle timeout — `AppState::idle_dim_delay`): drops
  the contrast register (`0x81`) to `oled_ui::DIM_CONTRAST`. Slows aging
  roughly in proportion to the current reduction, nothing more.
- **After `OFF_AFTER_DIM`** (5 minutes dimmed, `oled_ui.rs`) with no `Full` in
  between: `display-off` (`0xAE`). This is the tier that actually matters —
  zero segment current, so zero aging — and it's free to hold indefinitely
  since GDDRAM keeps the frame and `display-on` (`0xAF`) brings it back
  instantly with nothing to redraw.

`Full` (including `FullThenDimAfter`'s entry intent) cancels the timer,
restores `Variant::normal_contrast()`, and wakes the panel if it had gone
dark. The 5-minute threshold is a starting guess, not a measurement — tune it
once burn-in is actually visible on real hardware.

## 2.0" colour IPS TFT (Adafruit 4311)

ST7789 controller, 320×240, 4-wire SPI at 32 MHz. The breakout has an onboard
regulator and 3/5V level shifter, so the Pico drives it directly at 3.3V.

### Why a 1-bit framebuffer on a colour panel

A full RGB565 framebuffer is 320 × 240 × 2 = **150 KB**, and the dirty-diffing
every other backend uses needs a shadow copy too — 300 KB against the RP2040's
264 KB of total SRAM. It does not fit, and nothing clever makes it fit.

So `tft.rs` keeps the same 1-bit framebuffer the other panels use (9,600 bytes
plus a 9,600-byte shadow — *less* than `sharp.rs` spends) and expands it to
RGB565 one scanline at a time as it flushes. The whole shared layout stack,
the `DrawTarget<Color = BinaryColor>` contract and the dirty-region diffing
carry over untouched. Measured `.bss` bears this out: the TFT build uses
179 KB against the Sharp build's 194 KB.

The trade is one ink colour and one background colour per *frame* rather than
per pixel. Because the palette is applied at flush time, `tft_ui.rs` still uses
it for the thing colour is actually good at on a status readout — saying
"something needs attention" before you have read a word:

| Views | Ink |
| --- | --- |
| Status, recipe browser, bring-up screens | white |
| Next-stage prompt, stop confirmation, server offline, oven disconnected | amber |
| Recovery (post-crash) | red |

A palette change forces a full repaint, because the framebuffer diff tracks
bits, not the colours they currently stand for.

### Wiring

| Breakout pin | Pico W | Notes |
| --- | --- | --- |
| Vin (3–5V) | 3V3 (pin 36) | onboard regulator + level shifter |
| GND | GND | |
| SCK | GP18 (SPI0 SCK) | |
| MOSI | GP19 (SPI0 TX) | |
| CS | GP17 | active low |
| D/C | GP16 | |
| RST | GP20 | |
| BL | GP21 | PWM slice 2 channel B — physically next to RST (GP20/pin 26 and GP21/pin 27 are adjacent on the header) |
| MISO, SDCS | — | microSD only, unused |

Six signal pins now, one more than the OLED's five (it has no `BL`), so
swapping between those two panels means rewiring `BL` as well as a rebuild.

### If the picture comes out wrong

Two knobs, both in `MADCTL_LANDSCAPE` in `src/tft.rs`:

- **Upside down** — use `0x60` (MX instead of MY) to rotate 180°.
- **Red and blue swapped** — OR in `0x08` to select BGR order.

A photographic negative would mean the `INVON` in the init sequence is wrong
for your panel; these IPS units normally need it.

### Backlight

`BL` is driven by `backlight::PwmBacklightController` (GP21, PWM slice 2
channel B) and follows the same `BacklightPolicy` the FSM already computes
for the character LCD's RGB backlight — full while awake, dimmed to the same
`DEFAULT_DIM_LEVEL` after the idle timeout. The breakout pulls `BL` high
on-board, so the PWM duty cycle is un-inverted: 0 sinks the line low through
that pull-up, 255 drives it fully high.

## 2.7" Sharp Memory Display

SPI0 with an **active-high** CS on GP17, SCK on GP18, MOSI on GP19, at 2 MHz
(the panel's maximum). See `src/sharp.rs` for the panel's three protocol
quirks: LSB-first wire order, active-high CS, and the VCOM signal that must
alternate at ≥ 1 Hz or the image degrades.

## 16×2 character LCD

4-bit parallel bus on GP16–GP21 (RS=GP17, EN=GP16, D4–D7=GP21/GP20/GP19/GP18),
with the RGB backlight on the PWM pins GP6/GP7/GP8. This is the only backend
with a 3-channel RGB backlight — the TFT's single-channel `BL` is driven
separately (see above); the Sharp has no backlight hardware at all, so its
build gets a no-op `NullBacklightController`. These are selected by
`backlight::ActiveBacklight`, mirroring `screen::ActiveScreen`. The OLED also
gets `NullBacklightController` here (no `BL` pin to drive either), but it is
not backlight-blind — see "Burn-in mitigation" above for how it answers the
same `BacklightPolicy` over SPI instead of GPIO.
