# Sharp CS and bit order: why it's a GPIO and a `reverse_bits`

The Sharp Memory Display speaks a version of SPI the RP2040's hardware SPI block
cannot express: chip select is **active high**, and bytes go out **LSB-first**.
`sharp.rs` handles both in software — a plain GPIO for CS, `u8::reverse_bits` on
every byte — and this note records why the two hardware alternatives (an
external inverter on the PL022's CS, or a PIO implementation) were considered
and rejected.

**Status: nothing implemented, nothing changed.** The current approach is the
recommendation, not a stopgap. This exists so the next person doesn't spend a
board revision on the inverter.

## The mismatch

| | Sharp LS027B7DH01 | RP2040 PL022 |
| --- | --- | --- |
| CS polarity | active **high** | active **low** |
| Bit order | **LSB**-first | MSB-first only |
| Clock | mode 0, ≤ 2 MHz | mode 0 fine |

`sharp.rs` closes both gaps in software, at `flush`/`toggle_vcom`:

```rust
let _ = self.cs.set_high();     // active-high select
let r = self.spi.write(&tx[..p]);
let _ = self.cs.set_low();
```

with `tx` packed via `.reverse_bits()` per byte. Three lines and a method call.

## Option A: a hardware inverter on the PL022's CS

Wire a 74LVC1G04 between the PL022's CS output and the panel's SCS, so the
peripheral's active-low select arrives as active-high.

**The propagation-delay argument for it is sound but irrelevant.** A 74LVC1G04
at 3.3 V is ~3 ns typical, 5.5 ns worst case; `cs.set_high()` compiles to one
store to `SIO_GPIO_OUT_SET`, 8 ns at 125 MHz plus surrounding code. The gate is
several times faster. But the panel's CS timings are specified in
**microseconds** — tsSCS ≥ 3 µs of setup before the first SCLK rise, thSCS ≥ 1 µs
of hold, twSCSL ≥ 1 µs low between transfers. Nothing here is racing a GPIO
write. (Figures from the LS027B7DH01 datasheet; confirm against the part before
relying on them.)

If anything the hardware CS is *too* fast: the PL022 asserts its frame signal
about one SPI clock ahead of the first edge, which at 2 MHz is 500 ns — well
short of tsSCS.

**What actually kills it: the PL022 does not hold CS across frames in mode 0.**
With SPH=0 — `CaptureOnFirstTransition`, which is what the panel requires and
what `main.rs` sets — the PL022 returns its frame signal to idle between *every*
frame. It only holds the select across back-to-back frames at SPH=1, where the
leading clock edge delimits the frame instead of the CS edge.

Inverted, that turns a 12,001-byte flush into 12,001 separate transactions. SCS
falls after each byte, the panel reads every falling edge as end-of-transaction,
and each subsequent byte is interpreted as a fresh mode byte. Garbage, not a
frame.

SPH=1 isn't a way out: it's the wrong clock phase for a panel that latches on
the rising edge with SCLK idle low. And even there, "back-to-back" means the TX
FIFO never runs dry — 8 bytes is 32 µs of slack at 2 MHz, and any interrupt that
stalls a blocking write past that deasserts CS mid-frame. With cyw43 on PIO/DMA
alongside, that's a latent fault rather than a theoretical one.

Two smaller objections:

- **Reset glitch.** RP2040 pads come out of reset as inputs with pull-downs, so
  the inverter input reads low and drives SCS *high* from power-on until firmware
  claims the pin — the panel sits selected with no clock through boot. The GPIO
  version gets guaranteed-deselected for free.
- **It doesn't remove the software path.** The PL022 is MSB-first regardless, so
  `reverse_bits` stays either way.

**Verdict: no.** A part, a footprint and a boot glitch, in exchange for a
protocol violation.

## Option B: PIO

### The build story is lighter than it looks

PIO assembly is a separate language, but in Rust it is **not** a separate build
step. That expectation comes from the C SDK, where `pico_generate_pio_header()`
runs `pioasm` in CMake and emits a `.h` to include. In Rust it's a proc macro
assembling at compile time, inline in the source:

```rust
let prg = pio_proc::pio_asm!(
    ".side_set 1",              // SCLK
    "out pins, 1  side 0 [1]",  // drive MOSI, clock low
    "nop          side 1 [1]",  // panel latches on the rising edge
);
```

(Sketch, not compiled or run on hardware.) No generated artifact to check in, no
codegen in `build.rs` — which is four linker args and would stay that way.
`pio_proc::pio_file!("src/sharp.pio")` reads a real `.pio` file at compile time
if syntax highlighting is worth more than locality; still no build script.

`pio-proc` and `pio` 0.3.0 are **already in `Cargo.lock`**, pulled in
transitively by `cyw43-pio`.

### It would be one panel, not three

Only the Sharp has a protocol the PL022 can't express. `oled.rs` (SSD1305/1309)
and `tft.rs` (ST7789) are both active-low CS, MSB-first, mode 0 — exactly what
the PL022 is for, and at 32 MHz the TFT would be worse off on PIO. The display
features are mutually exclusive by construction (`screen.rs` const-asserts at
most one), so only one protocol is ever compiled in.

Resources are not a constraint: cyw43 owns PIO0, PIO1 is entirely free, and a
TX-only SPI program is 2-4 of its 32 instruction slots.

### What it would buy

Configure the output shift register right-shifting with autopull at 8 bits and
PIO shifts **LSB-first natively** — `reverse_bits` disappears from `sharp.rs`,
both the per-byte call in `flush` and quirk #1 in the module docs. Add `set pins`
for CS with delay cycles covering tsSCS/thSCS and the whole transaction including
chip select becomes one hardware operation, DMA-feedable.

### What it would cost

A timing-critical assembly program to own, a second peripheral-init path in
`main.rs`, and — if the DMA half is taken — the async cancellation hazard
documented in `pico-display-dma.md`, which doesn't change when the DMA source is
PIO instead of SPI0.

**Verdict: no, on cost/benefit rather than feasibility.** See the cheap
alternative below, which captures the LSB-first win with none of this.

## PIO is not slower than the PL022 — and speed isn't the axis anyway

Worth writing down because the intuition runs the other way. Both are hardware
shift registers clocked off the system clock, and both top out in the same place:

| Engine | Ceiling | Why |
| --- | --- | --- |
| PL022 | ~62.5 MHz | `SSPCLKOUT = clk_peri / (CPSDVSR × (1+SCR))`, `CPSDVSR` min 2 → 125/2 |
| PIO | ~62.5 MHz | 2 instructions per bit (`out`/`nop` with side-set) → 125/2 |
| CPU bit-bang | ~1-5 MHz | ~10-20 cycles per bit; no `RBIT` on ARMv6-M either |

The ordering is **PL022 ≈ PIO ≫ bit-bang**, not a descending staircase. Once
either hardware engine is configured the CPU does the same work: feed a FIFO, or
let DMA do it. The real tradeoff is expressiveness versus resources — PIO can
express any protocol in a few instructions but costs a state machine; the PL022
is fixed-function but free.

And for this panel it's moot twice over: SCLK is capped at 2 MHz, ~30× below what
either engine can drive. The bus is the bottleneck no matter what feeds it.

## What the software approach actually costs

Cortex-M0+ is ARMv6-M and has no `RBIT` instruction (that's ARMv7-M), so
`u8::reverse_bits` compiles to the SWAR sequence — roughly a dozen cycles per
byte. A worst-case 12,000-byte frame is therefore ~1-1.5 ms of reversal against
~48 ms of SPI at 2 MHz: **~2-3% overhead**, and single-digit microseconds on a
typical few-dirty-lines flush.

That is arithmetic from the instruction count, **not measured on hardware**.

## The cheap win, if it ever matters

Store the framebuffer already reversed instead of reversing at flush time:

- `set_pixel`'s mask becomes `0x80 >> (x % 8)` instead of `1u8 << (x % 8)`.
- `flush`'s inner loop becomes a `copy_from_slice` instead of a per-byte reverse.
- Line-address bytes still need reversing — 240 bytes instead of 12,000.
- The `last_sent` diff is unaffected (both sides change representation together),
  and `clear_white`'s `0xFF` is a palindrome.

That captures the entire LSB-first benefit listed for PIO, with no PIO, no state
machine and no assembly — which is a fair argument that the PIO route had little
left to offer here.

## Open item: tsSCS may be violated today

`flush` and `toggle_vcom` both go `set_high()` → `spi.write()` with nothing in
between — a few hundred nanoseconds at most, against a specified tsSCS of 3 µs.
It evidently works (these minimums are conservative), but if a flaky-panel bug
ever needs chasing, the free fix is `Timer::after_micros(3)` after the assert and
`(1)` before the deassert. Unverified against hardware; noted because it cuts
directly against the "we need hardware CS for speed" instinct — the panel wants
*more* delay, not less.

## Recommendation

Keep the GPIO CS and the PL022.

Driving CS from a GPIO isn't a Sharp-specific workaround in the first place —
`oled.rs` and `tft.rs` do the same, and `Spi::new_blocking_txonly` doesn't even
accept a CS pin, because nearly every real SPI device wants CS held across a
multi-byte transaction and the PL022 in mode 0 won't do it. The only thing
genuinely Sharp-specific is the polarity, which is one inverted call.

Revisit only if:

- The Sharp becomes the primary panel *and* full repaints get frequent enough
  that ~2-3% of flush time matters — in which case do the pre-reversed
  framebuffer first, not PIO.
- Some future panel needs a protocol the PL022 can't express *and* the timing
  budget is tight enough that software can't close the gap. That's the case PIO
  is actually for.
