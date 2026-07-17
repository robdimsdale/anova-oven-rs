# Pico OTA — flash ownership & the `memory.x` offset source

Status: **Decision + prototype committed** (`ee579c9`, not pushed). No behaviour change.
Date: 2026-05-29
Scope: `crates/anova-oven-pico` (RP2040), the OTA flash path in
[`src/ota.rs`](../crates/anova-oven-pico/src/ota.rs),
[`src/web.rs`](../crates/anova-oven-pico/src/web.rs),
[`build.rs`](../crates/anova-oven-pico/build.rs),
[`memory.x`](../crates/anova-oven-pico/memory.x).
Related: [`pico-ota.md`](pico-ota.md) (OTA feasibility/impl brief),
[`pico-crate-drift.md`](pico-crate-drift.md) (the broader "two copies must agree"
theme — the bootloader and app keep independent `memory.x` files).

## What this investigates

It started as "how do I de-duplicate the two `memory.x` files?" and became a
question about **how the app gets its DFU/STATE partition offsets** and **how the
`FLASH` peripheral is shared across tasks**. Those two are coupled, and the
coupling is the whole story.

The bootloader and app each have their own `memory.x` (this is the universal
real-world embassy-boot convention — see survey below; nobody shares them via a
crate or `INCLUDE`). The genuinely shared contract is just the **partition
address table** (BOOT2 / BOOTLOADER_STATE / ACTIVE / DFU / RAM). The app needs
the DFU and STATE offsets at runtime to build a `FirmwareUpdaterConfig`.

## The constraint chain (the crux)

1. embassy-boot's convenience constructor `FirmwareUpdaterConfig::from_linkerfile_blocking`
   hard-codes the flash mutex type to `NoopRawMutex`
   ([signature](https://docs.embassy.dev/embassy-boot/git/default/struct.FirmwareUpdaterConfig.html)).
2. `NoopRawMutex` is `Send` but **`!Sync`**
   ([docs](https://docs.embassy.dev/embassy-sync/git/default/blocking_mutex/raw/struct.NoopRawMutex.html)).
3. The blocking `Mutex<R, T>` is `Sync` only if `R: Sync`; `OnceLock<T>` (and any
   `static`) requires `T: Sync`. So `OnceLock<Mutex<NoopRawMutex, RefCell<Flash>>>`
   **cannot be a `static`** (verified against embassy-sync source).
4. The app's `ota` module is built around a **`static` flash singleton reachable
   from multiple tasks** (the `/update_firmware` web handler *and* the
   boot/health gate `mark_current_image_good`). To put it in a `static` it must
   be `Sync`, so it uses `CriticalSectionRawMutex` (which **is** `Sync`).
5. Because the cell is `CriticalSectionRawMutex`, `from_linkerfile_blocking`
   (which demands `NoopRawMutex`) is unusable → the app must supply the offsets
   itself → historically it parsed `memory.x` in `build.rs` and generated
   `partitions.rs` constants.

So `CriticalSectionRawMutex` is **not** over-engineering or a dual-core
requirement (an earlier claim in the discussion that was wrong — see "Corrections"):
it is precisely what makes the global, multi-task-reachable flash singleton sound
with zero `unsafe`. The `build.rs` parser was the price of that choice.

### Verified aside: reading linker symbols needs no `unsafe`

The DFU/STATE offsets equal the *addresses* of the `__bootloader_*` linker
symbols (the symbol has no contents). Taking that address via `addr_of!` does not
load from the extern static, so it needs no `unsafe` — unlike `&sym` or reading
`sym`. Confirmed by compile test:

| Form | `unsafe` required? |
|---|---|
| `core::ptr::addr_of!(SYM) as u32` | **No** |
| `&raw const SYM as u32` | **No** |
| `&SYM as *const u32 as u32` | Yes |
| `SYM` (read value) | Yes |

This is *cleaner than embassy's own* `from_linkerfile_blocking`, which uses the
`&SYM` form inside an `unsafe` block.

## The five options

1. **Shared static + `CriticalSectionRawMutex`, offsets via `build.rs` parser**
   (the original implementation). Global `OnceLock<Mutex<CriticalSectionRawMutex,
   RefCell<Flash>>>`; offsets parsed from `memory.x` into `partitions.rs`. No
   `unsafe`.
2. **`unsafe impl Sync` over a `NoopRawMutex` cell** (an earlier-history
   approach). Keeps `from_linkerfile_blocking` (no parser) but manually asserts
   `Sync` on a type embassy explicitly makes `!Sync`.
3. **`Peripherals::steal()` per request, local `NoopRawMutex`** (the
   [CroPDUster](https://github.com/9elements/CroPDUster) pattern). Re-acquires
   `FLASH` inside the handler; `from_linkerfile` works; one `unsafe` per call
   site.
4. **Client-pull, single-owner task.** One task owns `FLASH` *and* fetches the
   image (reqwless), writes, marks, reboots; other tasks send a zero-payload
   "mark good" signal. Structurally identical to embassy's own example
   ([application/rp/a.rs](https://github.com/embassy-rs/embassy/blob/main/examples/boot/application/rp/src/bin/a.rs)).
   No `static`, no `unsafe`, no parser.
5. **`StaticCell<Mutex<NoopRawMutex, …>>` + `from_linkerfile_blocking`** (the
   [mdrc-pacbot](https://github.com/RIT-MDRC/mdrc-pacbot) pattern). `StaticCell`'s
   audited `Sync` wrapper places a `NoopRawMutex` cell in a `static`; keeps
   `from_linkerfile_blocking` (no parser); no user `unsafe`.

A sixth, the **actor** (one owner task fronted by a channel `Sender` injected
into consumers), is a variant of 4 that also works for push — see ranking notes.

## Ranking & justification

Ranked best→worst **for this app** (RP2040, push-style OTA via a hosted picoserve
endpoint, two flash consumers, single thread-mode executor):

### 1st — Option 1, now parser-free via `addr_of!` (chosen)
The `CriticalSectionRawMutex` is *required* by the global, multi-task flash
singleton this app uses (constraint chain above), and it's the only thing that
makes that pattern sound with **zero `unsafe`**. The sole blemish — parsing
`memory.x` text in `build.rs` instead of using the linker's symbols — is removed
by reading the `__bootloader_*` symbols with `addr_of!` (zero `unsafe`, verified).
Lowest churn on a working, brick-if-wrong, hardware-only-testable path.
Empirically it's also the more common real-world idiom: the active embassy-boot
flash apps surveyed ([rusty-glove](https://github.com/simmsb/rusty-glove),
[CroPDUster](https://github.com/9elements/CroPDUster),
[mdrc-pacbot](https://github.com/RIT-MDRC/mdrc-pacbot)) use a shared static mutex
or local owner — none use an actor.

### 2nd — Option 4, client-pull single-owner
The most idiomatic *architecture* in the abstract — it's the embassy example's
own structure, and it **dissolves** the sharing problem: the fetcher is the
owner, so firmware bytes never cross a task boundary and the only cross-task
message is a zero-payload signal. No `static`, no `unsafe`, no parser. Ranked
below Option 1 only because it's a **rewrite of working push code** plus
server-side image hosting + version metadata, with little payoff at single-oven
scale. Becomes the clear winner if/when updates should be **server-driven**.
Reference: [embassy boot examples](https://github.com/embassy-rs/embassy/tree/main/examples/boot).

### 3rd — the actor (channel-fronted single owner)
Realises the "inject a thread-safe service everywhere" model cleanly: the
injected `Sender` is `Send + Sync` with no `unsafe`, contention is handled by the
queue, and single ownership lets the owner task use `from_linkerfile_blocking`
(no parser). The only path that is simultaneously injectable-everywhere,
`unsafe`-free, **and** parser-free under the push model. Ranked here, not 1st,
because the only real flash-via-actor precedent —
[drogue-device](https://github.com/drogue-iot/drogue-device)'s `FirmwareManager`
on [ector](https://github.com/drogue-iot/ector) /
[embedded-update](https://github.com/drogue-iot/embedded-update) — is niche
(ector ~68★) and dormant (drogue-device last commit Oct 2023), and it costs a
command enum + actor task + streaming chunks through the channel. (An earlier
claim that the actor is "the most idiomatic structure" was **not supported by
evidence** and is retracted — see Corrections.)

### 4th — Option 5, `StaticCell` + `NoopRawMutex` + `from_linkerfile_blocking`
Idiomatic and used by mdrc-pacbot on the same chip; deletes the parser with no
user `unsafe`. **But it does not fit this app's access pattern.** `NoopRawMutex`
is `!Sync`, so it can't be the global `OnceLock` the `ota` module exposes; you'd
refactor to plumb a `!Send` `&'static` into each consumer (picoserve `State` +
spawn-arg), all on one executor. And you'd *still* need `DFU_LENGTH` in Rust for
the pre-flight 413 check ([web.rs](../crates/anova-oven-pico/src/web.rs)), which
`from_linkerfile_blocking` doesn't expose — so the parser isn't fully deleted
anyway. It forces most of Option 4's restructuring while keeping the push model's
complexity: the worst trade of the lot.

### 5th — Option 3, `Peripherals::steal()` per request
Works (it's CroPDUster's real, shipping approach) but is `unsafe` and, with **two
consumers**, allows two live `Flash<FLASH>` handles to the same peripheral with
no compiler-checkable mutual exclusion. CroPDUster gets away with it because it
has a single, picoserve-serialised consumer; this app has two, so the same trick
is strictly riskier here. `steal()` per request is also not canonical embassy.

### 6th (last) — Option 2, `unsafe impl Sync` over `NoopRawMutex`
The worst. `NoopRawMutex` is `!Sync` *by design* to enforce single-executor use;
`unsafe impl Sync` re-asserts the exact guarantee the type exists to deny. Genuine
data-race risk if flash is ever touched from a second executor/interrupt. It's the
footgun embassy-sync's `RawMutex` zoo exists to prevent.

## What was done

- `61d47f1` — pruned the then-dead `__bootloader_*` symbols from the app's
  `memory.x` and fixed contradictory docs (the app had abandoned
  `from_linkerfile_blocking` but the docs still claimed it read those symbols).
- `ee579c9` — **Option 1 + `addr_of!`**: restored the `__bootloader_*` symbols
  (they have a real consumer again), deleted the ~90-line `build.rs` parser, and
  read the offsets via `addr_of!` in `ota.rs`. `DFU_PARTITION_SIZE` const →
  `dfu_partition_size()` fn (link-time value). Verified: links cleanly, `nm`
  shows the symbols at `0x8000 / 0x9000 / 0x100000 / 0x200000` — identical
  offsets/lengths to the old generated constants. Net −87 lines, no behaviour
  change, no `unsafe`.

**Open follow-up:** `mark_current_image_good` is written but not yet wired in
(dead-code warning); it is the second flash consumer the `static`-sharing design
exists for. Revisit Option 4 if/when OTA should be server-driven.

## Corrections made during the investigation (recorded for honesty)

- **"No public push-OTA + embassy-boot example exists"** — wrong;
  [CroPDUster](https://github.com/9elements/CroPDUster) is exactly that.
- **"Dual-core RP2040 justifies `CriticalSectionRawMutex` over `NoopRawMutex`"**
  — wrong; CroPDUster and mdrc-pacbot are the *same* dual-core chip and use
  `NoopRawMutex`. The actual hardware hazards (XIP-during-erase, core1) are
  handled by embassy-rp's `flash.rs` `in_ram()` itself (`critical_section::with`
  + `pause_core1()`), independent of the RawMutex. The real justification for
  `CriticalSectionRawMutex` is the `Sync`-for-`static` requirement (constraint
  chain), not the chip.
- **"The actor model is the most idiomatic structure"** — not supported; the only
  flash-via-actor precedent is niche and dormant, and the surveyed active apps use
  shared-mutex / local-owner.

## Real-world embassy-boot survey (all duplicate two `memory.x` files)

| Project | Chip | OTA receipt | Flash ownership |
|---|---|---|---|
| [CroPDUster](https://github.com/9elements/CroPDUster) | RP2040 | push (picoserve `POST /api/update`) | `steal()` local + `NoopRawMutex` + `from_linkerfile` |
| [mdrc-pacbot](https://github.com/RIT-MDRC/mdrc-pacbot) | RP2040 | network | `StaticCell<Mutex<NoopRawMutex,…>>` + `from_linkerfile_blocking` |
| [rusty-glove](https://github.com/simmsb/rusty-glove) | nRF52840 | BLE | bootloader uses `from_linkerfile_blocking` |
| this app | RP2040 | push (picoserve `/update_firmware`) | global `OnceLock<Mutex<CriticalSectionRawMutex,…>>` + `addr_of!` offsets |

None share `memory.x` across crates; duplication is the ecosystem norm
([embassy boot examples](https://github.com/embassy-rs/embassy/tree/main/examples/boot)).
No open embassy issue tracks the `from_linkerfile_blocking` `NoopRawMutex`
hard-coding (as of 2026-05).
