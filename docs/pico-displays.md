# Pico display backends

The firmware supports three panels plus a headless build. Exactly one is
compiled in, chosen by a Cargo feature, because they all claim pins from the
same GP16–GP21 block and each needs different peripheral init in `main.rs`.

| Build | Panel | Bus |
| --- | --- | --- |
| `cargo run --release` | none (headless, defmt logging only) | — |
| `--features ui-lcd` | 16×2 HD44780 character LCD | 4-bit parallel |
| `--features ui-sharp-basic` | Adafruit 2.7" Sharp Memory Display (LS027B7DH01, 400×240) | SPI0 |
| `--features ui-oled-basic` | Adafruit 2.42" OLED (product 2719, 128×64) | SPI0 |

`screen.rs` resolves the feature to `ActiveScreen` and const-asserts that at
most one is enabled; enabling two fails the build with that message.

## Code layers

The two graphical panels share everything above the wire:

```
ViewSpec                        (fsm.rs — what the FSM wants shown)
  └── view_plan::plan_view      (pico-core, host-tested: which lines, what
      │                          font role, centred or top-stacked)
      └── graphics_view         (fonts per panel-size tier, wrap measurement,
          │                      drop lines that don't fit, draw)
          ├── sharp.rs   → Sharp   (DrawTarget, dirty-line flush, VCOM upkeep)
          └── oled.rs    → Oled    (DrawTarget, dirty-page flush)
```

Adding a fourth panel means a driver implementing `DrawTarget<Color =
BinaryColor>`, a thin `*_ui.rs` backend implementing `DisplayBackend`, a
`ui-*` feature that pulls in `_graphics`, and one arm in `screen.rs`. The
layout code should not need to change: `graphics_view` picks its font tier
from the panel's reported size, and trims trailing lines that don't fit.

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
dropped — `fit_lines` trims from the end, and the planner orders a top-stacked
plan by importance, so what goes is what mattered least.

### Known gap: burn-in

An OLED dims where it has been lit longest, and this firmware shows a mostly
static status screen. Nothing mitigates that today. The natural fix reuses the
`BacklightPolicy` the FSM already computes (`state.rs` calls
`ctx.backlight.set_dim()` / `set_full()` at exactly the right moments): route
that to the display backend as well, and have the OLED backend answer it with
the controller's contrast command (`0x81`) or by parking the panel with
display-off (`0xAE`), which stops the ageing entirely and preserves GDDRAM.
That needs a second signal alongside `DisplayNotifier`, which is why it is not
in the initial backend.

## 2.7" Sharp Memory Display

SPI0 with an **active-high** CS on GP17, SCK on GP18, MOSI on GP19, at 2 MHz
(the panel's maximum). See `src/sharp.rs` for the panel's three protocol
quirks: LSB-first wire order, active-high CS, and the VCOM signal that must
alternate at ≥ 1 Hz or the image degrades.

## 16×2 character LCD

4-bit parallel bus on GP16–GP21 (RS=GP17, EN=GP16, D4–D7=GP21/GP20/GP19/GP18),
with the RGB backlight on the PWM pins GP6/GP7/GP8. This is the only backend
that uses the backlight controller.
