# Reset button: hardware (RUN pin) vs. software (GPIO)

A reset button lets a human force the Pico to reboot. There are two ways to
wire one, and they solve different problems. This note records the tradeoff so
the choice (and its diagnostic cost) is documented.

## Background: how resets interact with the persist code

`persist.rs` keeps crash/reset state in a `.uninit` SRAM region gated by a
`MAGIC` word. On RP2040 the core/SRAM supply stays up across every reset
*except* a true power cycle, so this data survives NRST (Negative reset), watchdog, and
`SCB::sys_reset()` — only a power-on (or bumped `MAGIC` after a firmware
update) is seen as `ColdBoot`.

`classify_reset()` attributes each boot to a `ResetReason` using
`WATCHDOG.REASON` plus our own breadcrumbs:

- panic handler advanced `panic_count` → `Panic`
- watchdog TIMER/FORCE bit set → `WatchdogTimeout` / `WatchdogForced`
- plain soft reset with `last_app_state` still in `INIT_STAGE_*` → `InitTimeout`
- anything else → `OtherSoftReset`

## Hardware button — RUN pin (pin 30) to ground

Tying RUN to ground (per the raspberrypi-spy reset-button design) asserts a
full chip reset in hardware, independent of CPU/firmware state. It is **not** a
power cycle: SRAM is retained, so the persist region survives and the boot is
**not** a `ColdBoot`.

Behavior after a press:

- `reset_count` increments; `panic_count` does not, so `message_is_new` is
  false and the LCD recovery view does **not** re-flash. The old panic message
  stays in `msg_buf` (probe-rs readable) but isn't re-shown.
- `WATCHDOG.REASON` is cleared by the reset (no TIMER/FORCE).
- Classified as `OtherSoftReset` if pressed while running normally, or
  **misclassified as `InitTimeout`** if pressed during WiFi/DHCP bring-up
  (`last_app_state` still `INIT_STAGE_*`).

Key property: it works **when the firmware is bricked** — HardFault loop,
interrupts disabled, executor deadlock, clock/peripheral misconfig, or a hang
during pre-`Watchdog::start()` bring-up. No software path can be relied on in
those states.

## Software button — GPIO + sentinel + `SCB::sys_reset()`

A normal GPIO input whose handler writes a sentinel into a persist field and
then soft-resets. This allows a dedicated `ResetReason` (e.g. `ButtonReset`)
so intentional operator resets are unambiguous in the ring buffer.

It only works if firmware is alive enough to service the GPIO and reach
`SCB::sys_reset()`. It does **not** help in the bricked cases above — which are
exactly when a human reaches for the button.

## Tradeoff summary

|                                                   | RUN-pin (HW)            | GPIO (SW)               |
| ------------------------------------------------- | ----------------------- | ----------------------- |
| Works when firmware is wedged / HardFaulting      | ✅                      | ❌                      |
| Works during pre-watchdog bring-up hang           | ✅                      | ❌                      |
| Distinguishable in reset history                  | ❌ (looks like `OtherSoftReset`) | ✅ (own `ResetReason`) |
| Preserves SRAM persist data                       | ✅                      | ✅                      |

The watchdog is the software net for hangs *after* it's started; the RUN pin
is the only mechanism for a genuine brick or a pre-watchdog lockup.

## Decision / recommendation

Use the **RUN-pin hardware button** as the recovery mechanism — it is the
right (and only reliable) tool for the bricked case. Its cost is that
`OtherSoftReset` in the reset ring becomes ambiguous: human press vs. spurious
soft reset are indistinguishable, since the RP2040 exposes no "RUN was pulled"
bit.

Add a software button **only if** labeled diagnostics for *intentional*
operator resets are also wanted, and never as the last-resort recovery path.

### Minor hazard (both, but realistic only with a button)

`record_and_reset` and `init_at_boot` do multi-word volatile writes that are
not atomic across the whole sequence. A press at exactly the wrong moment can
tear one record:

- mid-panic-handler (after `panic_count++`, before `msg_len`/`msg_buf`) → next
  boot shows `Panic` with a truncated/garbage message;
- mid-`init_at_boot` ring update → one garbled `ring_head`/ring entry.

The window is microseconds and the device is not bricked by it (magic stays
valid; one bad diagnostic record at worst). Most plausible during a
panic-reboot storm where a human is mashing the button.
