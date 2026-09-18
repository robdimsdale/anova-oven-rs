# Backlight wiring and OLED burn-in mitigation

Summary of the backlight work from this session, written for async review.
Covers what changed, why, and the reasoning behind each commit. See
`docs/pico-displays.md` for the wiring tables and per-panel notes this work
updated in place — this doc is the "why," that one is the "how to wire it."

## Status

| Change | State |
| --- | --- |
| TFT PWM backlight on GP21 + per-panel backlight gating | **Committed** (`a0ff2d5`), not pushed |
| OLED contrast/display-off burn-in mitigation | **Committed** (this branch), not pushed |

---

## 1. TFT backlight on GP21 (`a0ff2d5`)

### The ask

The TFT breakout (Adafruit 4311) has a `BL` pin the firmware was leaving
unconnected. `BL` is a PWM input, and the FSM already computes a
`BacklightPolicy` (full-brightness vs. dimmed-after-idle) for the character
LCD's RGB backlight — the TFT could use the same signal, it just had nothing
driving the pin.

### The wiring choice

`BL` → **GP21**, PWM slice 2 channel B. Two reasons this pin specifically:

- GP20 (already used for the TFT's `RST`) and GP21 sit on **adjacent physical
  pins** (26 and 27) on the Pico's header, so the new wire runs right next to
  one that's already there.
- GP20/GP21 also happen to share **PWM slice 2** (channel A / channel B), and
  that slice wasn't claimed by anything else — the character LCD's RGB
  backlight already uses slices 3 and 4 (GP6/7/8), so there's no conflict
  even though the LCD and TFT builds are mutually exclusive anyway.

The breakout pulls `BL` high on-board, so the PWM output is un-inverted:
duty 0 sinks the line low through that pull-up, duty 255 drives it fully
high — no inversion flag needed (unlike the LCD's RGB backlight, which does
invert, for reasons specific to that LED's wiring).

### The code structure

Added `backlight::ActiveBacklight`, a `cfg`-selected type alias that mirrors
the existing `screen::ActiveScreen` pattern (`screen.rs:15-42`) exactly, for
the same reason: the display panels can't coexist (they share the GP16–21
pin block), so which one is compiled in is a build-time choice already, and
the backlight hardware for the two now follows the same choice instead of
being a separate special case.

- `ui-lcd` → `BacklightController` (existing 3-channel RGB PWM, GP6/7/8,
  unchanged)
- `ui-tft-basic` → new `PwmBacklightController` (single-channel PWM, GP21)
- everything else (headless, `ui-sharp-basic`, `ui-oled-basic`) →
  new `NullBacklightController` — a no-op, because none of those three have
  backlight *hardware* to drive (Sharp is reflective; OLED is emissive and
  handled separately, see §2; headless has no panel at all)

Why a `cfg`-selected concrete type instead of a `dyn Backlight` trait object:
`display.rs`'s doc comment on `DisplayBackend` already states the project's
reasoning for the screen side — no `dyn`, no allocation, static dispatch,
because the executor is single-threaded and the backend is a compile-time
fact, not a runtime one. The same argument applies to the backlight, so it
uses the same mechanism rather than introducing a second pattern.

`main.rs` previously constructed the RGB `BacklightController` unconditionally
regardless of which panel feature was active — i.e. every build, including
headless, was driving GP6/7/8 PWM for hardware that was never wired unless
`ui-lcd` was the active feature. That's now gated per-feature like the screen
construction already was, so a non-LCD build no longer touches those pins at
all.

---

## 2. OLED burn-in mitigation (implemented, not committed)

### Why this isn't "wire up GP21 for the OLED too"

The OLED module (Adafruit 2719, SSD1305/SSD1309) has **no `BL` pin** — it's
self-emissive, not backlit (`docs/pico-displays.md`'s OLED wiring table has
nothing between D/C and D0). The original ask was to PWM-drive `BL` for both
the TFT and the OLED; for the OLED that request doesn't map onto real
hardware, so instead of building something that looked like the ask but
didn't correspond to a wire on the board, the equivalent mechanism for this
panel is a pair of *commands* to the SSD1305/1309 controller over the SPI bus
it already has: the contrast register (`0x81`) and display-off (`0xAE`).

### Why this needed more than a driver method

`OledScreen` (the concrete `ActiveScreen` for this feature) is owned
exclusively by `display_task`, a separate embassy task from the one that runs
the FSM and holds `Ctx`. `Ctx.backlight` — where the TFT/LCD backlight calls
happen — has no access to that task's SPI bus. So reaching the OLED's
contrast/off commands from `state.rs` needed a second cross-task channel,
mirroring the one that already carries `ViewSpec` to `display_task`:

- `display::BacklightNotifier` — a second `Signal`, alongside the existing
  `DisplayNotifier` (`display.rs:55`)
- `display_task` now `select3`s across the view signal, the backlight
  signal, and its render tick, instead of `select`ing across just the first
  and third (`display.rs:83-101`)
- `DisplayBackend` gained `set_backlight(&mut self, policy: BacklightPolicy)`
  with a **default no-op body** (`display.rs:29`) — so `LcdController`,
  `SharpScreen`, `TftScreen`, and `NullScreen` need no changes at all; only
  `OledScreen` overrides it
- `state.rs`'s `execute()` and `execute_idle()` now call
  `ctx.display.set_backlight(...)` alongside every existing
  `ctx.backlight.set_full()/set_dim()`/`apply()` call, so the OLED sees
  exactly the same policy transitions the LCD/TFT backlight hardware does

### The two-tier design, and why one tier isn't enough

OLED material ages from **cumulative current**, not a brightness threshold —
a dim pixel still ages, just slower. That means the FSM's existing `Dim`
policy (fires after 5s idle, `AppState::idle_dim_delay`) can only ever *slow*
aging if it's the only mechanism, never stop it. So `OledScreen` implements
two tiers instead of one (`oled_ui.rs:29-38, 92-126`):

1. **`Dim`** → contrast dropped to `DIM_CONTRAST = 0x01`, near the bottom of
   the register's range.
2. **Still dim after `OFF_AFTER_DIM` (5 minutes)** → `display-off` (`0xAE`).
   This is the tier that actually matters: zero segment current means zero
   aging, for as long as the panel stays off. It's free to hold indefinitely
   because GDDRAM keeps the last frame regardless of the display-off bit, so
   `display-on` brings the same image back instantly with nothing to redraw.

`Full` (and `FullThenDimAfter`'s entry intent, matching how the LCD/TFT
backlight already treat that variant) cancels the timer, restores the
variant's normal contrast (`Variant::normal_contrast()` — `0x32` for the
SSD1305 init table, `0x6F` for the SSD1309's), and wakes the panel if it had
gone dark.

### What's a guess vs. what's derived

`DIM_CONTRAST` and the 5-minute `OFF_AFTER_DIM` are starting points, not
measurements — there's no way to validate them without watching the actual
panel age, which isn't something I can do from here. Worth revisiting once
there's real hardware time on the board. Everything else in this section
(the two-tier structure, the choice of GDDRAM-preserving `0xAE` over a
cleared-frame approach, the cross-task signal) follows from the hardware's
actual behavior, not a tuning choice.

---

## 3. Burn-in across all four panels — what actually needs mitigating

Asked separately: do the other three panels have any comparable aging risk?
Short answer: no, and the reasons differ by panel.

**TFT (ST7789) and character LCD (HD44780)** — both are backlit
liquid-crystal, not emissive. The crystal itself doesn't degrade based on
what's displayed; that's an OLED-specific (organic-emitter) failure mode.
The one LCD aging mechanism that *does* exist — DC bias buildup from a
static drive pattern, seen as temporary image sticking — is already handled
by both controllers internally via routine frame-to-frame drive-polarity
inversion; nothing in this firmware needs to manage it, and it's transient
even in the rare case it shows up, unlike OLED's permanent material loss.
What *does* age here is the backlight LED itself (ordinary lumen
depreciation), but that's uniform across the whole panel — no ghosting — and
LED lifetimes at this scale (tens of thousands of hours to any noticeable
dimming) aren't the bottleneck for a kitchen device. `BacklightPolicy::Dim`
on these two panels is a UX/power/heat feature, not a burn-in mitigation.

**Sharp Memory LCD** is effectively immune by design — it's a reflective
memory LCD built for exactly this use case (long-static status content). The
same DC-bias mechanism applies in principle, but the panel is designed
around a VCOM signal that must alternate at ≥1 Hz specifically to prevent
it, and the firmware already does this correctly (`sharp.rs:12-14`,
`toggle_vcom`) — that code predates this session, it's called out here only
because it's the reason this panel needs nothing further. No backlight at
all (reflective, ambient-lit), so no LED-aging story either.

**Rough OLED numbers**, for scale (estimates for this class of part — small
white-emitter monochrome PMOLED — not a spec for this exact SKU, which
Adafruit doesn't publish):

- Unmitigated (static content, full brightness, no dimming): visible
  ghosting plausible in roughly **1,000–4,000 hours** — about 6 weeks to
  5–6 months of continuous full-brightness display.
- At `DIM_CONTRAST`'s near-minimum current: likely a **>10x** extension of
  that figure (aging scales with current, plausibly worse than linearly for
  OLED emitters), pushing the unmitigated-equivalent well past a year.
  Between dim and off (up to 5 minutes at a time) is the only phase where any
  aging still accumulates at all.
- At `display-off`: aging is arrested completely for as long as the panel
  stays off, which is most of any extended idle period once the off-timer
  fires.
