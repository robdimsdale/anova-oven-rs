# Display refresh cost, and a staged plan to cut it

The character LCD got a cell-span diff (`lcd_plan`, `LcdAnimator::tick`) because
its bus is pathologically slow. That prompted the obvious question of whether
the three *graphical* backends update as tightly. They mostly do at the flush
layer — but the flush layer is not where most of the cost is, and one of the
three spends its whole budget drawing into a panel that is switched off.

This document records the cost model, then a staged plan. Tasks are ordered so
each one is independently shippable and testable.

## The cost model

Two clocks matter and they give different answers.

**Transfer** — derived from byte counts and bus clocks, so these are solid:

| Panel | Bus | Smallest update | Full frame |
| --- | --- | --- | --- |
| Sharp | 2 MHz | line (400px × 1) — 52 B, **208 µs** | 12,482 B, **50 ms** |
| OLED | 4 MHz | page (128px × 8) — 131 B, **262 µs** | 1,051 B, **2.1 ms** |
| TFT | 32 MHz | row (320px × 1) — 651 B, **163 µs** | 153,611 B, **38 ms** |

Because every `spi.write` here is blocking on a cooperative executor, transfer
time *is* CPU time — the core spins feeding the FIFO and no other task advances.

**Rasterisation** — `render_view` clears the framebuffer and redraws every glyph
on every 50 ms tick, then the diff throws the result away. Order-of-magnitude
estimates from glyph areas and per-pixel instruction costs at 125 MHz, **not
measured — put a scope on this before trusting the figures**:

| Panel | clear | glyph draw | diff memcmp | per tick |
| --- | --- | --- | --- | --- |
| TFT | ~50 µs | ~800 µs | ~100 µs | **~1 ms** |
| Sharp | ~60 µs | ~800 µs | ~120 µs | **~1 ms** |
| OLED | ~10 µs | ~210 µs | ~15 µs | **~250 µs** |

Putting those together:

- **Aggregate occupancy, quiescent screen.** Rasterisation runs 20×/sec
  unconditionally: ~2% of wall clock on the TFT and Sharp. Transfer on an
  unchanging screen is ~0. Rasterisation dominates, and no flush optimisation
  can reach it — on an unchanged screen the flush already sends nothing and
  still pays the memcmp.
- **Worst-case single stall.** Transfer dominates absolutely: 38 ms (TFT) or
  50 ms (Sharp) against ~1 ms. This is the metric that drops button presses.

### Why the stall metric matters

`Debouncer` samples every 2 ms, needs 12 ms of charge to confirm a press, and
caps any single sample's contribution at `MAX_STEP_MS` = 6 ms
(`pico-core/src/button.rs`). A stall **under ~6 ms cannot distort it** — the
sample lands late, carries its true measured elapsed time, and stays under the
cap. Longer, and the weight is truncated. Long enough to span a whole 20–35 ms
tap, and the press is lost outright.

A full repaint is 6–8× the cap. On the TFT this is not rare: `set_palette`
forces `force_full`, and `palette_for` changes white→amber on
`Status` → `StopConfirmation` / `NextStagePrompt`. Those are *encoder-driven*
transitions, so full repaints are correlated with the user pressing things.

## Scope

In scope, in order:

0a. *(optional, hardware)* Move the Sharp's VCOM to `EXTCOMIN` (Sharp only) —
    do this first if at all, because it deletes work from tasks 1 and 4
0b. **Periodic full refresh on the Sharp** (Sharp only) — a spec violation that
    exists today, not an optimisation. Low severity, cheap fix; task 1 must not
    defeat it.
1. Stop re-rendering unchanged frames (all three graphical panels)
2. Stop rendering into a dark OLED (OLED only)
3. Column-bounded flush (TFT only)
4. Chunked transactions with yield points (TFT and Sharp)
5. Stream the Sharp transfer instead of packing 12.5 KB of stack (Sharp only)
6. Record the outcome in `docs/pico-display-dma.md`

Explicitly **out of scope**:

- **DMA.** See task 4's closing note — tasks 4 and 5 are its prerequisite, and
  the revisit criteria in `docs/pico-display-dma.md` still hold.
- **OLED column bounds.** Technically available (page addressing lets you set a
  column start and write a short span), but the whole frame is 2.1 ms. Best case
  saves a few hundred microseconds. Not worth the code.
- **Sharp line-span bounds.** Impossible — the protocol addresses a line and
  then requires all 50 bytes of it. `sharp.rs` is already at the protocol floor.
- **The TFT/WiFi dropouts.** Under investigation separately and almost certainly
  a power-rail problem (the TFT's ~60–100 mA on the Pico's 3V3 pin, shared with
  the CYW43439), not a rendering problem. Do not conflate the two.

---

## Task 0a — optional hardware change: move the Sharp's VCOM to `EXTCOMIN`

**Sharp only. Needs one wire and one jumper. Deletes work from tasks 1 and 4,
so do it first or not at all.**

All panel figures in this task are from the
[LS027B7DH01 datasheet](https://cdn-learn.adafruit.com/assets/assets/000/094/215/original/LS027B7DH01_Rev_Jun_2010.pdf?1597872422)
(Rev. Jun 2010), tables 6-3-1 / 6-3-2 and remarks 4-1 / 4-2. Note the
LS013B4DN04 datasheet also floating around Adafruit's CDN is a **different
part** (1.3", 96×96) — do not take timings from it.

The Sharp panel requires VCOM to alternate at ≥ 1 Hz or it accumulates a DC bias
and the image degrades. Today that is done *in software*: VCOM is bit 1 of the
SPI mode byte (`CMD_VCOM = 0x02`), so every flush carries the current parity and
`SharpScreen::render` issues a 2-byte `toggle_vcom` no-op on ticks where nothing
changed. That is why the render path cannot simply be skipped (see task 1's
trap) and why chunked writes have to hold VCOM constant across chunks (task 4).

The panel also supports driving VCOM from a hardware square wave on `EXTCOMIN`,
selected by strapping `EXTMODE` high. On the Adafruit 2.7" breakout
(**product 4694**) all three of `EXIN`, `EXTMD` and `DISP` are broken out and
currently unconnected.

Since the panel works today with all three floating, the board evidently pulls
`EXTMD` **low** (serial VCOM, what the firmware drives) and `DISP` **high**
(display enabled). Confirm against the board before wiring.

### Design

| Pin | Connect to | Why |
| --- | --- | --- |
| `EXTMD` | 3V3 | selects `EXTCOMIN` as the VCOM source |
| `EXIN` | **GP21** (PWM slice 2 channel B) | the VCOM square wave |
| `DISP` | leave unconnected | already pulled high; blanking has no use on a reflective panel with no burn-in risk |

**GP21 is the TFT's `BL` pin.** Reuse it rather than claiming a fresh GPIO: the
display features are mutually exclusive by construction (`screen.rs`
const-asserts at most one), GP21 sits at the end of the GP16–GP21 block the
panels already occupy so it stays on one header, and it keeps a single mental
model — *GP21 is the display's PWM line, whatever that means for the panel
that's fitted*. Swapping between the Sharp and the TFT then moves the same
jumper rather than adding one.

Nothing contends for it in a Sharp build: `ActiveBacklight` resolves to
`NullBacklightController`, which takes no peripherals, so `PWM_SLICE2` and
`PIN_21` are both free for the VCOM output.

#### Picking the frequency

Three specs bound it, and they are tighter than they first look:

| Spec | Code | Value |
| --- | --- | --- |
| EXTCOMIN signal frequency | `fEXTCOMIN` | TYP **1 Hz**, MAX **20 Hz** |
| COM frequency (what EXTCOMIN drives) | `fCOM` | MIN **0.5 Hz**, MAX **10 Hz** |
| EXTCOMIN high pulse width | `twEXTCOMIN` | MIN **1 µs** |

So the usable window is **0.5–10 Hz**, with 1 Hz the typical operating point.
Do not use 10 Hz — that is the `fCOM` ceiling, not a target.

This constrains the implementation, because **the RP2040's PWM cannot reach
1 Hz**. Its floor is 125 MHz ÷ 255.94 ÷ 65536 ≈ **7.45 Hz** (maximum divider,
maximum TOP). That is in spec, but 7× the typical. So there is a real trade:

| | Frequency | Survives a firmware panic? |
| --- | --- | --- |
| PWM slice at the floor | ~7.45 Hz | **yes** — hardware keeps running |
| `loop { pin.toggle(); Timer::after_millis(500).await }` | 1 Hz (typical) | no — dies with the executor |

**Prefer the PWM slice, but not strongly.** 7.45 Hz is legal and well inside
`fCOM`, and hardware that keeps running through a firmware fault is nice to
have. It is a weaker argument than it first looks, though: losing VCOM is
slow-acting and recoverable (see "How bad is losing VCOM?" below), and a
panicked device is getting rebooted anyway — so the fault-survival case is worth
something, not much. Either implementation is defensible. Configure once at
boot; nothing touches it again. Every PWM slice is free in a Sharp build — there
is no backlight.

The toggle task is the fallback if 7.45 Hz ever proves to be a problem, and is
what prompted this section. It works because the pin is independent of the SPI
bus — unlike a task trying to drive the *serial* VCOM bit, which would need a
`Mutex<Sharp>` shared with the display task and would gain nothing on a
cooperative executor.

#### Other ways to generate it

There are two separate axes here, and it is worth not conflating them as an
earlier draft of this section did: **what frequency** an option reaches, and
**what failures it survives**. The frequency axis turns out not to matter. The
independence axis does, a little.

| Option | Reaches 1 Hz | Parts | Survives a bricked MCU |
| --- | --- | --- | --- |
| **PWM slice** *(chosen)* | no — 7.45 Hz | none | no |
| Toggle task | yes | none | no — dies with the executor |
| PIO1 program | yes, exactly | none | no |
| PWM + external ÷N | yes at ÷8 (÷4 → 1.86 Hz, also legal) | flip-flops or ripple counter | **no** — downstream of the PWM, so it inherits every MCU failure |
| External CMOS 555 | yes | 555 + 2R + 1C | **yes** |

PIO1 is the pick if exact 1 Hz is ever wanted without parts: 125 MHz ÷ 65536
gives a 1907 Hz instruction rate, then a delay loop. The block is entirely
unused (cyw43 owns PIO0) and `pio-proc` is already in `Cargo.lock`.

Two objections that would normally rule out a 555 **do not apply here**:

- *Duty cycle.* A classic astable cannot do 50% without a diode trick. Irrelevant
  — the only duty constraint is `twEXTCOMIN` ≥ **1 µs**, and at 1 Hz even a 10%
  duty gives 100 ms.
- *Accuracy.* RC drift with temperature and tolerance is large, but the legal
  window is **0.5–10 Hz** — a 20× span. Nothing in a 555's error budget comes
  close to leaving it.

(If a 555 were used it should be a CMOS part — TLC555 / ICM7555 / LMC555 — not a
bipolar NE555: 3.3V native, ~100 µA instead of several mA, and tolerant of the
MΩ-scale resistors a 1 Hz RC needs.)

**The frequency axis is settled by power.** The only reason to prefer 1 Hz would
be reducing COM-inversion current. Table 6-4 puts the entire panel at **50 µW
typical / 250 µW max** with no data update, and **175 µW / 350 µW** while
updating at 1 Hz. Running VCOM 7.5× faster moves a fraction of a fraction of a
milliwatt on a mains-powered oven controller. There is nothing to save, so
7.45 Hz is not a compromise — it is simply fine.

#### The independence argument, which is the real one

The 555's actual case is not frequency, it is that **nothing else here survives
an MCU that never runs your code**. A firmware panic leaves PWM and PIO
running; a watchdog reset loop only gaps them briefly. But a bad flash, a
corrupt image, or a board left sitting in BOOTSEL means the PWM is never
configured at all — and the Sharp retains its last frame for as long as VDD is
present. Powered, static image, no VCOM, indefinitely: the worst-case
combination.

That is not an exotic fault on a project being reflashed constantly. A dev board
in BOOTSEL overnight, or running a build from before this feature existed, hits
it by accident.

Weighed honestly, it is still a **real but minor** problem: the consequence is
recoverable ghosting (below), bounded by how long a non-functioning board stays
powered. On a breadboard a TLC555 plus two resistors and a cap is near-zero
friction; on a final build it is BOM and board area for a self-clearing effect.

**Start with the PWM slice** — free, in spec, and the failure it does not cover
is recoverable. Reach for the 555 if a panel ever shows real retention after a
long BOOTSEL session, or if this moves to a board where the MCU and the panel
have independent lifecycles.

**One unresolved point.** Remark 6-3 says *"EXTCOMIN frequency should be made
lower than frame frequency,"* and Frame Frequency (`fSCS`) is specced MIN 1 Hz.
On a static screen no frames are sent at all, so that relationship inverts. The
reading here is that 6-3 is guidance for continuous-update operation rather than
a hard rule under `EXTMODE=H` — but that is an inference, not something the
datasheet states. Worth confirming on the bench, and a reason not to sit at the
top of the range.

### What this deletes

- `Sharp::toggle_vcom`, the `vcom` field, `CMD_VCOM`, and the parity handling in
  `flush` — the mode byte becomes a constant `CMD_WRITE`
- `SharpScreen::render`'s three-arm match on `flush`'s return value collapses to
  a plain flush; `flush`'s `Ok(bool)` loses its only consumer
- **Task 1's `idle_tick()` hook is no longer needed** — gating the render stops
  being dangerous, and the trait keeps its current shape
- **Task 4's "hold `self.vcom` constant across chunks" caveat disappears**
- Quirk #3 leaves the `sharp.rs` module docs, leaving the active-high CS and the
  LSB-first wire order

### How bad is losing VCOM?

Recoverable, not destructive — worth knowing before treating this as urgent.

`fCOM` (MIN 0.5 Hz) is specified in the *driving timing* table, so violating it
falls under operating precaution (2), *"may not have proper picture quality"* —
not precaution (1)'s *"may be damaged."* The datasheet reserves damage language
for out-of-spec **electrical** operation, disassembly, UV and out-of-range
storage temperature, and does not use it here.

The mechanism (general LC physics, not stated in the datasheet) is ionic
impurities migrating to the alignment layers under sustained DC and building a
counter-field, seen as ghosting and washed-out contrast. It reverses once AC
drive resumes, over minutes to hours. Prolonged severe DC stress *can* degrade
the alignment layer permanently, but that is a far harsher exposure than a
firmware bug caught within days.

It is still the more serious of the two static-image failure modes — continuous,
whole-panel, and the only one with even a distant path to permanent damage.

### What this does *not* fix

`EXTCOMIN` handles VCOM alternation only. It does **not** satisfy the panel's
separate requirement to rewrite the pixel data periodically — see task 0b, which
is needed whether or not this task is done.

### ⚠ Hardware and firmware must land together

Once `EXTMD` is high the serial VCOM bit does nothing. Tying it high **without**
the PWM in place stops VCOM entirely. Wire and flash in the same sitting, and if
you revert the firmware, pull `EXTMD` back down.

### Verification

Build matrix only; this is bin-side. Confirm with a scope or logic analyser that
`EXIN` is toggling before trusting it, then leave the panel on a static screen
for an extended period and check for image degradation — that is the only real
test, and it is slow.

### Acceptance

`EXIN` toggles at ~7.45 Hz from boot, independent of anything the display task
does. No VCOM state remains in `sharp.rs`. A static screen is stable over hours.

---

## Task 0b — periodic full refresh on the Sharp

**Sharp only. A spec violation that exists today, not an optimisation.
Independent of task 0a — needed either way. Low severity but cheap to fix; see
"How bad is it?" below before prioritising it.**

The [LS027B7DH01 datasheet](https://cdn-learn.adafruit.com/assets/assets/000/094/215/original/LS027B7DH01_Rev_Jun_2010.pdf?1597872422)
"For operating LCD module" precautions say, in full:

> **(3)** A still image should be displayed less than two hours, if it is
> necessary to display still image longer than two hours, display image data
> must be refreshed in order to avoid sticking image on LCD panel.
>
> **(4)** If LCD module takes a static electricity, as the display image which
> is written into pixel memory might not be displayed, Data update should be
> executed frequently.

VCOM alternation is therefore **necessary but not sufficient**. The panel also
requires the *pixel data itself* to be rewritten periodically, and the two
requirements are independent: `toggle_vcom` (or `EXTCOMIN`) satisfies neither
precaution above.

### How bad is it?

**Temporary, and you would probably never see it.** Sharp calls the outcome a
"sticking image" and attaches none of the damage or non-recovery language it
uses elsewhere in the same document — *"may be damaged"* for out-of-spec
electrical operation, *"permanent damage"* for disassembly, *"may not recover
its original condition"* for out-of-range storage temperature. The remedy is to
do the thing that was missing: write new data, and the retention clears.

Precaution (4) is not degradation at all. ESD can corrupt the per-pixel SRAM so
the panel shows the wrong thing; the next full write fixes it completely. This
task gets that for free.

What is not stated anywhere is the *mechanism* for (3). With VCOM alternating,
the gross DC is already cancelled, so this is presumably residual polarity
asymmetry accumulating over hours — but that is inference, not datasheet. What
is solid: Sharp measured it, put two hours on it, and prescribed rewriting the
data.

Three reasons to do it anyway, despite the low severity:

- **The violation is not marginal.** An idle oven overnight is 8–12 hours
  against a two-hour limit — routinely 4–6× over, not occasionally grazing it.
- **It hides from you.** The ghost would be the idle screen, and the idle screen
  is what this device displays almost all the time. It would only surface as a
  shadow behind some *other* view — the moment you are least likely to be
  studying the panel closely.
- **The fix is one timer** against an effect that is slow, cumulative and hard
  to observe.

### This is already broken

`flush` sends only the lines that differ from `last_sent`, so on a screen that
is not changing, **the panel's pixel memory is never rewritten** — the only
traffic is the 2-byte VCOM no-op. An oven left idle overnight is precisely the
case precaution (3) warns about, and this is true of the code as it stands,
before anything in this plan is done.

Task 1 (gating the render) does not cause this, but it must not be allowed to
mask it either.

### Design

Force a full repaint on a timer: set `force_full = true` every **30 minutes**.
One extra 50 ms full flush per half hour — negligible cost, and it also clears
any pixel-memory corruption per precaution (4).

Thirty minutes rather than the datasheet's two-hour limit, for margin: a device
that repeatedly wakes and re-idles, or a timer that drifts, should not be able
to creep past the real bound.

Where the timer lives depends on task 1. It must fire **regardless of whether
the render is gated** — the point is to repaint when nothing has changed, which
is exactly when the gate would otherwise suppress everything. Putting it in
`SharpScreen` (checked on each `render`) is wrong for that reason unless
`render` is still called every tick. Under task 1's deadline model this is
clean: the refresh is simply another entry in `next_deadline()`, and firing it
means "render, and force a full flush" rather than a special case that has to
defeat a gate.

### Does this apply to the other panels?

No, and for different reasons each time. The OLED is emissive with no
liquid-crystal sticking mechanism (its own aging problem is handled by task 2
and `docs/pico-backlight.md` §2). The TFT is actively refreshed from GRAM by the
ST7789 many times a second, so its pixel data is inherently never static at the
panel level. Only the Sharp holds a frame in per-pixel memory and leaves it
there untouched.

### Verification

Host-testable if the interval logic lives in `pico-core` alongside task 1's
gate — assert that a full refresh is requested at the interval even when the
view and age are unchanged. The panel-level effect needs hardware and is slow to
observe.

### Acceptance

A Sharp build left on a static screen issues a full-frame flush at least every
30 minutes. Verifiable on a logic analyser without waiting for image sticking:
look for the ~12 KB transfer.

---

## Task 1 — stop re-rendering unchanged frames

**Biggest win, smallest change, helps all three panels.**

The render output is a pure function of `(view, timer_age_secs)`.
`ViewSpec::timer_age_secs` returns *whole seconds*, and 0 for everything except a
`Status` carrying a `timer_anchor` (`fsm.rs:297`). So every non-`Status` screen —
`Connecting`, `ServerOffline`, `RecipeBrowser`, the prompts, `Recovery` — is
completely static between view changes, yet re-rasterises 20×/sec. The idle
screen, which this device shows most of its life, redraws a 60px `fub49_tn`
temperature twenty times a second to produce identical pixels.

Note `timer_anchor` is `Some` for *any* status once the first one lands —
`install_status` anchors on first sight and only re-anchors when the reading
moves — so on an idle oven with a parked timer the age still increments each
second, even though the planner ignores it (`timer_remaining_secs_after` returns
`None` unless `timer_mode == "running"`). A gate keyed on age would therefore still fire
1×/sec on the idle screen. The deadline model sidesteps this by keying off
`timer_mode == "running"` directly rather than off the age — see below.

**Files:** `pico/src/display.rs` (primarily), `pico/src/sharp_ui.rs`.

### Design: compute the next deadline, don't poll for it

The 50 ms tick is polling for a condition that already arrives as an event. Two
of `display_task`'s three `select3` arms are *already* signals (`notifier`,
`backlight_notifier`); only `Timer::after(ANIM_TICK_MS)` polls. So rather than
keeping the tick and gating the render behind a comparison, replace the tick
with **the next instant anything is actually due**:

```rust
let next = [
    running_timer.then(|| next_second_boundary),
    animator.next_step_at(),   // ui-lcd only; None when nothing is scrolling
    sharp_refresh_due_at,      // task 0b
].into_iter().flatten().min();

match select3(notifier.wait(), backlight_notifier.wait(), maybe_timer(next)).await
```

With nothing pending, the third arm is `Never` and the task sleeps until
something signals it. A new `ViewSpec` always renders; otherwise the wake came
from a deadline that knows why it fired.

This subsumes the gate-and-compare approach, and it is better in three ways:
`next_deadline()` is a pure function and therefore host-testable; task 0b's
refresh becomes a scheduled deadline rather than an override bolted onto a gate;
and the LCD's animation stops being an accident of the tick rate.

**Do not add `PartialEq` to `ViewSpec`.** It derives only `Clone` (`fsm.rs:243`)
and holds `String`, `OvenStatus` and `CurrentCook`. Nothing above needs it — a
signalled view is rendered unconditionally.

### Three things genuinely need time, not events

Enumerate these before writing the deadline function; missing one freezes
something.

1. **The cook timer countdown** — 1 Hz, and only while `timer_mode ==
   "running"`. Pure time, no external event.
2. **The character LCD's marquee** — `LcdController::render` drives
   `LcdAnimator::tick(&plan, now)`, whose constants are `SCROLL_STEP` 350 ms,
   `END_PAUSE` 1200 ms and `MIN_SLOT_HOLD` 3000 ms (`lcd_plan.rs`). An
   event-only loop would freeze a marquee mid-scroll. This is `ui-lcd` **only**
   — the graphical panels have no animation at all and can be purely
   event-driven. `LcdAnimator` already takes `now` rather than reading the
   clock, so exposing a `next_step_at()` alongside `tick` fits how it is built.
3. **The Sharp's 30-minute refresh** — task 0b.

(VCOM was a fourth until task 0a; `EXTCOMIN` removes it from software entirely.)

### How much does this actually save?

**Be clear about what is being bought.** Not re-rasterising is the win: ~1 ms ×
20/sec ≈ **2% of wall clock**. Removing the *wakeup* on top of that saves a
timer interrupt, an executor poll and a comparison — a few µs, 20×/sec, call it
**0.01%**. The performance is in the first part; the deadline model is for
clarity and testability.

So do not expect a measurable power or CPU difference between "gate the render"
and "compute the deadline." Choose the deadline model because it is easier to
reason about and does not need a trap-door for task 0b, not because it is
faster.

### Where to stop

One layer down, `execute_idle` calls `display.render(...)` on every
`api_changed()`, so the notifier fires ~1 Hz even when the polled data is
identical — meaning the task wakes about once a second regardless. Suppressing
*that* needs equality on `ApiSnapshot` or `ViewSpec`, which is exactly the
machinery this task avoids.

**Not worth it.** Waking once a second to discover nothing changed costs a
comparison, and the expensive part is already gated. Chasing the last wakeup
buys ~0.01% for real complexity. (If the push architecture under discussion
separately ever lands, this stops being a question — the notifier would fire
only on actual change.)

### ⚠ `display_heartbeat` will start lying

`persist::bump_display_heartbeat()` is called on every render, so today
"advancing" means "the display task is alive", and `/health` serves it.

Once the task legitimately sleeps for minutes, a frozen counter becomes
ambiguous between *correctly idle* and *wedged* — the diagnostic inverts from
useful to misleading exactly when you would reach for it. **Bump it on wake
rather than on render**, or record a last-wake timestamp instead of a counter.

The watchdog itself is unaffected: `watchdog_feeder_task` runs on its own
`WATCHDOG_FEED_INTERVAL_SECS` timer and never consults the display heartbeat, so
sleeping the display task cannot reset the device.

### Worth doing alongside: make `timer_age_secs` honest

`timer_age_secs` is only ever *consumed* by a running timer, but it counts up
for any `Status` (see the note above). The deadline model no longer depends on
that — it schedules from `timer_mode` — but the value is still misleading to
the next reader.

Making it return 0 unless the timer is running (`timer_mode == "running" &&
timer_total_secs > 0`, matching `timer_remaining_secs_after`) costs nothing and
removes the discrepancy. Separate commit from the deadline work so a regression
is bisectable, with a host test asserting an idle `Status` reports age 0 while a
running one still counts.

### ⚠ Trap: this will break the Sharp if done naively

The Sharp's VCOM upkeep lives *inside* `SharpScreen::render` — `flush()` returns
whether it sent anything, and on `Ok(false)` the backend issues the cheap 2-byte
`toggle_vcom`. Skipping `render` entirely stops VCOM alternating, the panel
accumulates a DC bias, and **the image degrades**. This is the one hard
requirement the panel places on us (`sharp.rs:12-14`).

Add a `DisplayBackend::idle_tick()` method with a default no-op body — mirroring
how `set_backlight` already defaults for backends that don't need it — and call
it from `display_task` on every tick where the render is skipped. `SharpScreen`
implements it as `toggle_vcom`. The OLED and TFT keep the default: an emissive
panel and an actively-refreshed TFT have no VCOM to maintain.

**If task 0a has been done, skip all of this.** With VCOM on `EXTCOMIN` there is
nothing to maintain from software, no hook is needed, and `DisplayBackend` keeps
its current shape.

### The Sharp's data refresh is a deadline, not a trap

Under a gate-and-compare design, task 0b's periodic full refresh would be a
hazard: it must fire precisely when nothing has changed, which is exactly what
such a gate suppresses, so it would need a bolted-on override.

The deadline model removes that entirely — the refresh is just another entry in
`next_deadline()`, and firing it means "render, and force a full flush". This is
the main reason to prefer the deadline model over gating: the awkward case stops
being special.

Still cover it in the tests below. A missed refresh is mild and reversible
(task 0b's "How bad is it?") but invisible from the code and slow to show on the
bench, which is what a test is for.

### Verification

- The gating predicate is pure. Consider lifting it into `pico-core` with host
  tests (precedent: `LcdAnimator::tick` takes `now` rather than reading the
  clock, so its timing is host-testable).
- Cases to cover: new view → renders; same view, same second → skipped; same
  view, next second, running timer → renders; the task 0b interval elapsing with
  everything else unchanged → renders *and* forces a full flush; a non-`Status`
  view (age always 0)
  across many ticks → skipped throughout. With the optional refinement, add:
  idle `Status` across many ticks → skipped.
- `scripts/build-and-test.sh` must stay clean across all six configs.

### Acceptance

Quiescent idle screen issues ~1 render/sec, not 20 (0 with the refinement).
A static non-`Status` screen issues none. Sharp VCOM still toggles at 20 Hz. A
running cook timer still advances once per second.

---

## Task 2 — stop rendering into a dark OLED

**OLED only. Largest single saving available on that panel.**

`OledScreen::render` deliberately keeps drawing and flushing while `panel_off`
is set, on the reasoning that GDDRAM retains the frame so `display_on` brings it
back with nothing to redraw. That benefit is real but the trade is badly priced:
`panel_off` is the steady state after `OFF_AFTER_DIM` (5 minutes idle), i.e. most
of this device's life. It costs ~250 µs of rasterisation 20×/sec — ~5 ms/sec,
indefinitely — plus flushes, all into a panel nobody can see, to save a single
~250 µs render and at most one 2.1 ms flush, once, at wake.

**Files:** `pico/src/oled_ui.rs`.

### Design

Skip `render_view` and `flush` while `self.panel_off`. On wake, in
`set_backlight`'s `Full` arm, after `display_on()`, either force a render
inline or let the next 50 ms tick repaint — a ≤50 ms stale frame at wake is
imperceptible, and the simpler option is preferable.

Note task 1 interacts: with the render gated, a wake must mark the frame dirty
so the next tick actually redraws rather than being skipped again. Whichever
mechanism task 1 lands on, `set_backlight(Full)` must set it.

### Verification

Build matrix only — `oled_ui.rs` is bin-only and cannot be host-tested. Review
carefully and confirm the wake path sets the dirty flag. Flag for hardware
confirmation (see the end of this document).

### Acceptance

While `panel_off`, no rasterisation and no SPI traffic except the backlight
commands. Waking from `display-off` shows a current frame within one tick.

---

## Task 3 — column-bounded flush on the TFT

**TFT only. 2–8× on the common path.**

`write_rows` always sets `CASET` to the full `0..=319` (`tft.rs:223`), so a dirty
row costs all 640 bytes however few pixels in it changed. The ST7789 supports a
sub-width window and the GRAM pointer *wraps within it*, so a rectangle remains
one `CASET`/`RASET`/`RAMWR` with no per-row re-addressing.

Expected, using the current font tiers:

| Case | Today | Bounded |
| --- | --- | --- |
| Idle temperature, one digit changes (~40px of 320) | 38,400 B / 9.6 ms | 4,800 B / **1.2 ms** |
| Idle temperature, whole number (~155px) | 38,400 B / 9.6 ms | 18,600 B / **4.7 ms** |
| Cooking timer row (helvB14, ~18 rows, ~100px) | 11,520 B / 2.9 ms | 1,800 B / **0.45 ms** |

**Files:** `pico/src/tft.rs`, optionally `pico-core`.

### Design

Extend the existing row-run diff to also track a column span. For each run of
contiguous dirty rows, compute the union of changed *columns* across that run,
then address only that sub-rectangle.

Bounds must be the union across the run, not per row — the window is rectangular.
A run where two widely separated things changed will bound wider than the sum of
its parts; that is correct and still no worse than today.

The bit-span diff (`&[u8]` row vs `&[u8]` shadow → `Option<(first, last)>`) is
pure and dependency-free. **Strongly consider putting it in `pico-core` with
tests**, following `lcd_plan`'s precedent — its cell-span diff lives there with a
panel model in the tests that reconstructs what the panel would show. A wrong
column bound corrupts the display *silently*, which is exactly the failure the
LCD tests were written to catch.

### Verification

- Host tests on the span function: no change → `None`; single pixel; whole row;
  changes at bit 0 and bit 7 of a byte (the off-by-one trap); multi-row union.
- Ideally a panel model like `lcd_plan`'s: apply the spans to a model
  framebuffer and assert it reconstructs the intended frame exactly.

### Acceptance

A one-digit idle temperature change sends ~4,800 bytes, not 38,400. Full repaints
are unaffected (still full width). No stale pixels under the span tests.

---

## Task 4 — chunked transactions with yield points

**TFT and Sharp. Fixes dropped button presses. Does not reduce work.**

This converts one 38 ms (TFT) or 50 ms (Sharp) stall into a series of ~1.3 ms
ones, so the button sampler's 2 ms timer actually gets polled. The hardware timer
interrupt already fires during a blocking write — the wake is never delayed, only
the *poll*. That is what the yield restores.

Note `flush` currently coalesces contiguous dirty rows into a single
`write_rows` call, so a full repaint is **one run of 240 rows**. Yielding between
*runs* therefore yields zero times in exactly the case that matters. The yield
must go between chunks *within* a run.

**Files:** `pico/src/tft.rs`, `pico/src/sharp.rs`, `pico/src/tft_ui.rs`,
`pico/src/sharp_ui.rs`.

### Design

Make `flush` async and split each run into chunks, each a **complete,
self-contained transaction** with CS released at its boundary:

```rust
pub async fn flush(&mut self) -> Result<bool, SPI::Error> {
    // ...find run [start, run_end) as today...
    let mut y0 = start;
    while y0 < run_end {
        let y1 = (y0 + rows_per_chunk).min(run_end);
        self.write_rows(y0, y1)?;              // self-contained; CS released on exit
        let (f, t) = (y0 * BYTES_PER_ROW, y1 * BYTES_PER_ROW);
        self.last_sent[f..t].copy_from_slice(&self.frame[f..t]);
        yield_now().await;                      // CS deasserted — safe cancellation point
        y0 = y1;
    }
}
```

`write_rows` already re-issues `CASET`/`RASET`/`RAMWR` per call, so chunking
costs ~11 command bytes (~2.75 µs) per chunk — about 82 µs across a full repaint.
Committing the shadow per chunk rather than per run is strictly better on the
error path; it already works this way, just at coarser granularity.

**Size chunks by byte budget, not row count** — after task 3 a row's cost varies
with the column span. Target ≤~1.3 ms per chunk:

| Panel | Bus | Budget | Full-width chunk |
| --- | --- | --- | --- |
| TFT | 32 MHz | ~5,200 B | 8 rows (1.28 ms) |
| Sharp | 2 MHz | ~320 B | 6 lines (1.25 ms) |

`embassy_futures::yield_now()` wakes its own waker and returns `Pending` once.
Because the task re-queues itself the executor will not `wfe()` mid-flush; it
drains the run queue — picking up anything the timer interrupt woke — and comes
straight back.

Making `flush` async ripples only as far as the backends' `render`, which is
already `async`.

### Sharp specifics

The Sharp packs everything into one buffer and does a single `spi.write`, so it
has no natural chunk boundary — which is why this task pairs with task 5 and they
should land together.

You cannot deassert CS mid-frame (the mode byte and frame-end byte bracket the
transfer), but you *can* send each chunk as its own complete frame — mode byte,
N lines, frame-end — since a single-line update is a valid Sharp frame. Two extra
bytes per chunk, ~8 µs at 2 MHz.

**Keep `self.vcom` constant across all chunks of one frame and toggle it once
after the last**, or a full repaint will flip VCOM 40 times. (Moot if task 0a has
been done — there is no VCOM bit left to manage.)

### ⚠ Cancellation

This reintroduces the hazard `docs/pico-display-dma.md` describes — a dropped
future mid-flush — minus the DMA. It is inert today because `display_task`
awaits `render` on its own line, *outside* its `select3`, so the future is never
dropped. It becomes live the moment anyone wraps `render` in a timeout or select.

The chunked structure is what makes it safe rather than merely unlikely: at every
await point CS is deasserted and the panel is in a consistent state, so a dropped
future leaves the panel partially updated, the shadow accurate for what landed,
and the rest still dirty for the next tick to retry.

**Put a comment on the yield point saying exactly this**, so a future reader
does not "simplify" it by hoisting the CS assertion out of the chunk loop.

### Note for whoever later considers DMA

Tasks 4 and 5 are the DMA prerequisite, not an alternative to it. The difference
is where the await sits: *between* transactions here (safe by construction),
*inside* one with DMA (needs a drop guard, because `spi.write(..).await` is
itself the suspension point). DMA would additionally need ping-pong scanline
buffers on the TFT — the 1-bit→RGB565 expansion is ~35 µs against 160 µs of
transfer, so without overlapping it you recover only part of the row time.

What DMA buys beyond this task is CPU *throughput* (~2–5% duty cycle), not
latency. Nothing needs that headroom today. Doing this task shortens the DMA
conversion considerably if it ever does.

### Could PIO own the transaction instead?

Asked while writing this up: if a PIO state machine drove CS itself, the frame
would be atomic in hardware and CS could never be left asserted. Recorded
because the idea is sound, the answer is interesting, and one form of it works.

PIO can certainly sequence CS. Each state machine has separate pin groups for
`out`, `set` and side-set, so MOSI / CS / SCK map onto the three, and a frame
carries its own length in-band. Sketch, untested:

```
.program spi_cs_tx
.side_set 1                      ; SCK
.wrap_target
    pull block        side 0     ; first word of a frame = bit count - 1
    mov  x, osr       side 0
    set  pins, 0      side 0     ; assert CS
bitloop:
    out  pins, 1      side 0     ; MOSI, SCK low
    jmp  x--, bitloop side 1     ; SCK high, clock it in
    set  pins, 1      side 0     ; count exhausted -> deassert CS
.wrap
```

Two instructions per bit, so the same ~62.5 MHz ceiling the PL022 has.

**By itself it does not make a transfer uninterruptible.** A PIO TX FIFO is 4
words — 16 bytes, or 32 with `FJOIN_TX` stealing the RX side — against a 640-byte
TFT row. Stop feeding it mid-frame, which is exactly what a dropped future
means, and the SM stalls on an empty FIFO with the counter non-zero and CS still
asserted. The invalid state has moved from the CPU to the PIO, not gone. (Note
`SM_RESTART` alone will not recover it: it clears shift counters and stalled
instructions but not the PC or X/Y, so you would inject a `set pins` to force CS
idle.)

**The form that does work is PIO CS plus a DMA that completes on drop rather
than aborting.** With a software CS that policy is useless — a completed DMA
still leaves CS asserted, because the CPU owns the pin and is not coming back to
raise it. With CS deassertion as the last instruction of the PIO frame,
completion releases it by construction. The driver marks itself busy and the next
call waits on the in-flight frame. That is a genuinely atomic transaction.

This is a gap in `docs/pico-sharp-cs.md`, which priced abort-on-drop and
concluded the hazard "doesn't change when the DMA source is PIO instead of
SPI0." True for abort, false for complete-on-drop, and it is PIO's ownership of
CS that makes the latter a viable policy at all. That doc has been corrected.

Two things to keep separate from this task either way:

- **PIO is not a latency fix.** Without DMA the CPU still feeds the FIFO word by
  word and blocks for the same ~38 ms. PIO changes who drives the pins, not who
  moves the bytes. Tasks 4 and 5 are orthogonal and still needed.
- **The RGB565 expansion stays on the CPU.** PIO has no clean way to hold two
  16-bit palette entries and select between them per bit, so even full PIO+DMA
  leaves ~20% of each row's cost on the core without ping-pong buffers.

Per panel, the case splits the wrong way for this plan's purposes. The **Sharp**
gains a lot — LSB-first natively (deleting `reverse_bits`), active-high CS as a
polarity choice, and `set pins` delay cycles enforcing the panel's 3 µs `tsSCS`
exactly, which closes the open item at the end of `pico-sharp-cs.md`. The
**TFT** gains essentially nothing: active-low CS, MSB-first, mode 0 is precisely
what the PL022 is for. The panel with the 38 ms stall is the one PIO does not
help.

**Verdict: not part of this plan.** For the hazard as it stands, a `Drop` impl
holding `&mut CS` that raises the line is ~5 lines and reliable; the
panel-left-mid-command half is already covered by task 4's shadow-commit
discipline. PIO is the right tool if the Sharp's `reverse_bits` and CS quirk are
ever worth deleting, ideally alongside DMA — not as a first move against the TFT
stall.

### Verification

- Chunk-boundary arithmetic is pure — host-test it: exact multiples, remainders,
  single-row runs, a full-height run.
- Confirm by inspection that CS is deasserted at every await point.
- Confirm the Sharp toggles VCOM exactly once per frame regardless of chunk count.

### Acceptance

No single blocking interval in a full repaint exceeds ~1.5 ms. Total flush time
grows by <1%. Frames render identically.

---

## Task 5 — stream the Sharp transfer

**Sharp only. Lands with task 4.**

`flush` builds the whole transfer into `let mut tx = [0u8; TX_LEN]` — **12,482
bytes of stack, zeroed on every call**, including a call that sends one 52-byte
line. `tft.rs` already avoids this deliberately and its module docs call it out:
"640 bytes of stack, against the ~12 KB `sharp.rs` puts there for the same job."

**Files:** `pico/src/sharp.rs`.

### Design

Drop the buffer. Assert CS, write the mode byte, write each dirty line as it is
found, write the frame-end byte, deassert. Gaps between `spi.write` calls are
harmless with a GPIO CS held high. Per-call overhead is negligible against 208 µs
of transfer per line.

With task 4's chunking this becomes: one transaction per chunk, each with its own
mode byte and frame-end, with the VCOM caveat above.

### Verification

Build matrix; confirm stack high-water mark drops (`llvm-size` on `.bss` will not
show this — it is stack, not static, so reason about it or instrument if you have
a probe attached).

### Acceptance

No multi-kilobyte stack allocation in the flush path. Frames render identically.

---

## Task 6 — update the docs

`docs/pico-display-dma.md` currently concludes "not yet, and not for the reason
previously written down," with revisit triggers. After tasks 4 and 5 that
conclusion still holds but its shape changes: the cancellation-safe transaction
structure now *exists*, so the remaining DMA work is the drop guard and the
ping-pong buffers. Add a section saying so, and make the point explicitly that
the chunk-and-yield approach and DMA differ in where the await sits.

Two of that document's premises have also drifted and should be corrected while
you are in there:

1. Its starvation evidence is "~5.3–5.9 ms against a 5 ms nominal."
   `Debouncer::SAMPLE_INTERVAL_MS` is now **2 ms**, so that measurement no longer
   supports the conclusion unchanged.
2. "The ~38-48 ms worst case only happens on a full repaint, which is rare by
   construction" holds for the Sharp and OLED but **not** the TFT, where
   `set_palette` forces a full repaint on encoder-driven view transitions.

`docs/pico-displays.md` should gain a short note on the render gate and the
column-bounded flush.

---

## Verification across the whole plan

`scripts/build-and-test.sh` must stay clean: `cargo fmt --check`, clippy
`-D warnings`, the host test suite (151 tests at time of writing), and all six
firmware configs.

Prefer putting pure logic in `pico-core` where it can be host-tested — the span
diff (task 3) and the chunk arithmetic (task 4) both qualify, and `lcd_plan`'s
panel-model tests are the pattern to copy. Driver code that touches the bus stays
in the bin and is covered by the build matrix and review only.

## What needs hardware

None of this can be fully confirmed on the host. Flag for bench verification:

- **Task 0a** — that `EXIN` toggles from boot, and that a static screen is stable
  over hours. Also the task most easily got wrong *silently*: if `EXTMD` goes
  high without the PWM running, VCOM stops and nothing in software will tell
  you. Not destructive — the effect is recoverable ghosting, see "How bad is
  losing VCOM?" — but it is invisible until it is visible, so scope the pin
  rather than assuming.
- **Task 1** — that no screen freezes, and (if task 0a was skipped) that the
  Sharp image does not degrade over a long idle period — the VCOM trap.
- **Task 2** — that the OLED wakes showing current data.
- **Task 3** — no stale pixels after partial updates. This is the highest-risk
  task visually; a wrong column bound corrupts silently.
- **Task 4** — that quick taps are no longer dropped during a view transition on
  the TFT. This is the whole point of the task and is the one thing only hardware
  can confirm.

The rasterisation figures in the cost model above are estimates. If a probe is
attached, measuring the actual per-tick render cost before and after task 1 is
worth doing — it is the number the largest claim in this document rests on.
