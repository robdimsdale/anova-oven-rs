# Display SPI: why it's blocking, and what DMA would actually cost

The firmware drives every SPI panel with blocking transfers. Five comments in
the tree explained that as a hardware/HAL constraint — cyw43 owning the only
DMA interrupt. That explanation is wrong. This note records what the constraint
actually is, so the next person doesn't route around a problem that isn't there,
and writes down the one real hazard (async cancellation) that the compiler will
*not* catch for you.

**Status: nothing implemented.** The erroneous comments are corrected; the
drivers are unchanged and still blocking. DMA remains a deliberate "not yet".

## The claim, and where it came from

Five places said some variant of "async DMA would need a second DMA channel's
`DMA_IRQ_0` binding, which cyw43 already owns exclusively":

| Location | Form |
| --- | --- |
| `sharp.rs` module docs | The original, and the one the others cite |
| `oled.rs` module docs | Copy |
| `main.rs` ×3 (Sharp, OLED, TFT init) | Copy |
| `Cargo.toml` (`embedded-graphics` dep) | "see sharp.rs for why not async/DMA" |

Tracing it back:

- It first appears in `a308cd1` *"WIP: add sharp display driver"* — a WIP commit
  with an empty body and no rationale.
- `84d42ab` and `77f85cd` copied it into the OLED and TFT backends.
- **No code ever tried it**: `git log --all -S"new_txonly"` returns nothing on
  any branch. There is no reverted attempt, no failing experiment.
- **No doc ever discussed it**: `grep -rn DMA docs/` was empty before this file.
- **It isn't an old-API artifact**: `embassy-rp` has been pinned at `0.10.0`
  since the initial commit (`959c210`), so the comment was written against the
  same API in the tree today.

Five citations, one uncorroborated source, never tested. Worth remembering as a
failure mode in its own right: a rationale written in a WIP commit acquires
authority purely by being copied.

## Why it's wrong

Three facts about `embassy-rp` 0.10, all checkable in the vendored source:

1. **`bind_interrupts!` takes a list of handlers per interrupt.** The macro
   grammar is `$irq:ident => $($handler:ty),*;` — it generates one ISR that
   calls each handler's `on_interrupt()` in turn and implements `Binding` for
   each. Nothing about a binding is exclusive.
2. **`dma::InterruptHandler<T>` is per-channel and minds its own business.** It
   tests `ints0 & (1 << channel)` and clears only its own bit before waking only
   its own waker. Two channels' handlers on the same IRQ do not interfere.
3. **`Channel::new` uses `write_set` on `inte(0)`**, so enabling a second
   channel ORs its bit in rather than clobbering cyw43's.

Every DMA channel routes to `DMA_IRQ_0` (`channel!(DMA_CH1, 1, DMA_IRQ_0)`),
so sharing that interrupt was never optional for anyone — it is the design.

### The compile proof

Adding the binding and switching the OLED to async SPI:

```rust
bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<PIO0>;
    DMA_IRQ_0 => embassy_rp::dma::InterruptHandler<DMA_CH0>,
                 embassy_rp::dma::InterruptHandler<DMA_CH1>;
});

let spi = embassy_rp::spi::Spi::new_txonly(
    p.SPI0, p.PIN_18, p.PIN_19, p.DMA_CH1, Irqs, spi_cfg,
);
```

produces exactly one error:

```
expected `Spi<'_, SPI0, Blocking>`, found `Spi<'_, SPI0, Async>`
```

That is `OledScreen::new`'s signature complaining — the *driver's* type, not the
interrupt binding. The binding and `new_txonly` both type-check.

## The real blocker

The drivers. `sharp.rs`, `oled.rs` and `tft.rs` are local modules taking an
`embedded_hal::spi::SpiBus`, and their `flush()` is synchronous. Converting them
means making `flush` async and awaiting `Spi<Async>::write`. That is mechanical,
and the plumbing above it already exists — `DisplayBackend::render` is already
`async fn`, so nothing in `display_task` or the layout stack has to change.

The cost is the work plus the hazard below, not the HAL.

## The hazard: async cancellation

This is the part to understand before anyone starts, because **the Rust compiler
will not catch it.**

### No, the compiler cannot help here

Dropping a future is always safe and always allowed in Rust. There is no linear
or `!Drop` type to forbid it, no `must_complete` bound, and no lint. Wrapping a
render in `with_timeout` or racing it in a `select` compiles clean and produces
no warning. The type system has no concept of cancellation safety at all.

Nor is this a memory-safety problem that `unsafe` review would surface.
`embassy-rp`'s `Transfer::drop` aborts the channel and spins until
`!ctrl_trig().busy()`, so the DMA is provably stopped before the source buffer
dies. The borrow checker's job — keeping `tx` alive for the duration of
`write(&tx)` — is already done correctly.

What breaks is *protocol state*, which no compiler models. The failure is
silent, visual, and looks like a hardware fault.

### Failure modes if a render future is dropped mid-transfer

Today's `flush()` implementations already get the hardest part right: all three
commit their `last_sent` shadow **only after** a successful write, so abandoned
rows stay marked dirty and are resent on the next tick. Those self-heal. The
ones that don't:

| Failure | Mechanism | How it looks |
| --- | --- | --- |
| **CS left asserted** | `sharp.rs` does `cs.set_high(); spi.write(...); cs.set_low();`. Drop the future at the await and the third line never runs. | The next flush's `set_high()` is a no-op, so the panel treats the new frame as a continuation of the old transaction. Its mode byte is eaten as line data — persistent garbage that does *not* self-heal. |
| **Panel left mid-command** | The TFT writes a window-set command then streams pixels; the OLED writes three addressing commands then a page. An abort between them leaves the controller expecting data. | The next command sequence is interpreted as pixel data. Colour/window desync, often persistent until reset. |
| **D/C pin left in data state** | Same shape as CS, for the panels that toggle D/C mid-flush. | Subsequent commands are silently treated as data. |
| **Partial line records** (Sharp) | The panel latches per line record; a truncated one may write a corrupt line. | One bad row. Self-heals, since that row stays dirty. |

Note what is *not* on this list: no UB, no watchdog trip, no panic, nothing in
the defmt log. The display task stays healthy and keeps rendering. You find out
by looking at the panel.

### How to avoid it

1. **Keep `render()` off any cancellation point.** `display_task` currently
   awaits `screen.render(&current)` *outside* its `select3`, which is why this
   is safe today. Treat that as a load-bearing invariant, not an accident —
   the tempting future change is wrapping render in a `with_timeout`, and that
   is precisely the change that introduces the bug.
2. **If a timeout is ever genuinely needed, make the driver survive abandonment
   rather than trying to prevent it:**
   - Hold CS (and D/C) in an RAII guard whose `Drop` restores the idle state, so
     an abandoned transfer can't leave the bus asserted.
   - Set a `needs_resync` flag before the transfer and clear it after. A flush
     that starts with the flag set re-issues the full command sequence and forces
     a full repaint (`force_full = true`) instead of assuming the panel's state.
   - Keep the existing commit-after-success discipline for `last_sent`.
3. **Prefer an outer watchdog to an inner timeout.** Letting a flush run to
   completion and catching a genuinely wedged bus at the watchdog is safer than
   cancelling a transfer mid-transaction, given the 8 s budget and a worst-case
   flush measured in tens of milliseconds.

## Which panels would be worth converting

Transfer times computed from the configured clocks (see the commit messages for
`77f85cd` and `84d42ab`), not measured on hardware:

| Panel | Clock | Worst-case flush | Per transfer | Verdict |
| --- | --- | --- | --- | --- |
| TFT ST7789 (`ui-tft-basic`) | 32 MHz | ~38 ms | 640 B scanline | **Best candidate.** Biggest window, and it currently serialises RGB565 expansion with the transfer — overlapping them cuts wall-clock time as well as CPU. Needs a second scanline buffer (~1.2 KB). |
| Sharp (`ui-sharp-basic`) | 2 MHz | ~48 ms | one packed buffer, up to ~12 KB | **Worth it.** Entirely clock-bound, so DMA frees essentially the whole window, and `flush` already packs every dirty line into a single `spi.write` — the ideal shape for one transfer. But the typical dirty-line flush is already sub-millisecond. |
| OLED (`ui-oled-basic`) | 4 MHz | ~2 ms | 128 B page | **No.** Adding async machinery to reclaim 2 ms. |
| HD44780 (`ui-lcd`) | — | — | — | **N/A.** 4-bit parallel GPIO, no SPI. |

Per-transfer sizes are all far above the point where DMA setup overhead (a few
µs) matters — even the Sharp's smallest useful transfer takes hundreds of µs to
clock out at 2 MHz. The overhead argument against DMA doesn't hold; the driver
rewrite and the cancellation hazard are the real costs.

## Would it let the CPU sleep more? Would power drop?

Technically yes; measurably no.

**The mechanism is real.** With a blocking transfer the core sits in a polling
loop feeding the SPI FIFO for the whole flush — it cannot reach the executor's
`wfe()`. With DMA the transfer proceeds without the core, so that time becomes
schedulable, and idle if nothing else is ready. So DMA genuinely converts busy
time into potential sleep time.

**The arithmetic kills it.** Three multipliers, each pushing the same way:

1. **Duty cycle.** `display_task` ticks every 50 ms, and the dirty-region
   diffing means a typical flush is sub-millisecond on the Sharp and ~2 ms on
   the OLED. That's roughly 1-4% of wall-clock spent inside a transfer. The
   ~38-48 ms worst case only happens on a full repaint, which is rare by
   construction.
2. **The core is a minority of system draw.** This is a Pico W with the radio
   associated and `PowerManagementMode::None` set explicitly at
   `main.rs:379` — the CYW43439 is kept fully awake and dominates consumption by
   a wide margin. Saving core cycles nibbles at the smaller term.
3. **`wfe()` is not deep sleep.** It stops clocking the cores; PLLs, XIP, SRAM
   and peripherals stay up. The delta between running and WFE is a fraction of
   core draw, not all of it.

Multiply a fraction of a minority by a few percent of duty cycle and the result
is a small fraction of one percent of system power — below what you could
distinguish without a lab supply and a controlled test. (Figures here are
order-of-magnitude reasoning from the configured clocks and the parts involved,
not bench measurements. Measure before believing any of it.)

Two things worth being clear about:

- **DMA does not reduce the transfer's own energy.** The SPI peripheral clocks
  out the same bits at the same rate either way. Only the core's participation
  changes.
- **The flush is not what keeps the CPU awake.** The executor only idles when
  *no* task is ready, and between the 50 ms render tick, the cyw43 task, the
  network stack and the poll scheduler, something usually is. Removing the
  transfer from the critical path doesn't create long idle windows; it widens
  short ones.

If power ever does become a goal, the levers in descending order of effect are
all elsewhere:

| Lever | Where | Rough scale |
| --- | --- | --- |
| cyw43 `PowerManagementMode::None` → `PowerSave` | `main.rs:379` | Dominant term — this is the one that matters |
| Lengthen `ANIM_TICK_MS` when nothing is animating | `display.rs:11` | 20 wakeups/sec for a screen that usually hasn't changed |
| Poll cadence | `scheduler` in pico-core | Radio airtime, so it compounds with the first row |
| Display DMA | this doc | Noise floor |

For a mains-powered oven controller, none of this is a design constraint.
**Don't convert to DMA for power reasons** — if it's ever worth doing, it's for
CPU headroom.

## Recommendation

Not yet, and not for the reason previously written down.

It buys neither responsiveness nor power (see above). The measurement that
prompted this investigation showed the input tasks are **not** being starved: the button sampler was hitting ~5.3-5.9 ms against a 5 ms
nominal while the display task was rendering every 50 ms (see §1.6 of
`pico-review.md`). So DMA today buys CPU headroom, not responsiveness, and
there is no latency problem for it to solve.

Revisit if any of these change:

- Input latency measurably suffers — the button task's rejection log prints
  sample counts precisely so starvation is visible when it happens.
- The TFT becomes the primary panel and full repaints get frequent.
- Something else needs the ~38-48 ms of CPU a worst-case repaint spends.

If it does get built: **TFT first**, keep the OLED blocking, and treat the
cancellation-safety section above as the design constraint rather than an
afterthought.
