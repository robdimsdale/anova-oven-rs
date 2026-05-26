# Drift between `pico-core` and `pico` (bin)

A field-by-field reference for where the same data appears in both crates
(and in `scripts/dump-persist.sh`), what currently prevents the copies from
falling out of sync, and the silent-drift gaps that remain.

Read this when:

- Adding a field to the persist region — there's a checklist in §6.
- Investigating a `/health` response that's missing data the firmware is
  clearly recording.
- Investigating a `dump-persist.sh` decode that disagrees with `/health`.
- Considering a port of the firmware to a different MCU family (the bin's
  chip-specific layer is where most drift surfaces live).

## 1. Crates and what they own

- **`anova-oven-pico-core`** (`crates/anova-oven-pico-core`) — pure-logic
  library. Host-testable via plain `cargo test`. No MMIO, no chip HAL, no
  radio driver. Defines the decoded data shapes (`Snapshot`,
  `ResetHistoryEntry`, `AppStateLabel`, `Heartbeats`, `ResetReason`) and
  the name lookups (`ResetReason::name`, `app_state_name`,
  `init_stage_name`). See `crates/anova-oven-pico-core/src/persist_data.rs`
  and `crates/anova-oven-pico-core/src/reset.rs`.
- **`anova-oven-pico`** (`crates/anova-oven-pico`) — `thumbv6m-none-eabi`
  firmware bin. Owns the chip-specific MMIO layer: `PersistRegion`,
  `RingEntry`, the `.uninit.PERSIST` static, the panic handler, the
  WATCHDOG.REASON MMIO read, the watchdog feeder, the `/health` HTTP
  server. See `crates/anova-oven-pico/src/persist.rs` and
  `crates/anova-oven-pico/src/health.rs`.
- **`scripts/dump-persist.sh`** (in the bin's `scripts/`) — debug-port
  tool that reads the persist region over SWD with `probe-rs` and decodes
  it. Parses constants, struct field layouts, and name tables out of the
  Rust source so it doesn't carry its own copies (this script's whole
  decode used to be hand-maintained and silently rotted — see the
  header comment in the script for the history).

## 2. The drift surfaces — overview

Most of the historical drift surfaces have been eliminated by moving the
canonical definition into `pico-core` and `pub use`-ing it from the bin.
What remains is the boundary between the **on-MMIO `#[repr(C)]` layout
types** (in the bin, all `u32` for atomic word access) and the **decoded
typed views** (in `pico-core`, with bools/enums/`heapless::String`). Those
two views must agree on field count, field order, and field meaning, but
the type system can't directly verify that.

The sections below detail each surface, ordered from "structurally
impossible to drift" to "silent if you forget".

## 3. Eliminated drift surfaces — single source of truth

| Item | Canonical location | Used from |
|---|---|---|
| `MSG_BUF_SIZE`, `RING_SIZE` | `pico-core::persist_data` | bin re-exports at [persist.rs:65](../crates/anova-oven-pico/src/persist.rs#L65) |
| `Snapshot`, `ResetHistoryEntry`, `AppStateLabel`, `Heartbeats` | `pico-core::persist_data` | bin re-exports at [persist.rs:88](../crates/anova-oven-pico/src/persist.rs#L88); `/health` serializes `Snapshot` directly (its derived `Serialize` impl *is* the response schema) |
| `ResetReason`, `INIT_STAGE_*` consts, `classify_reset` | `pico-core::reset` | bin re-exports at [persist.rs:116](../crates/anova-oven-pico/src/persist.rs#L116) |
| `ResetReason → name` mapping | `ResetReason::name()` in `pico-core::reset` (exhaustive match — adding a variant fails to compile) | `/health` JSON (via `Serialize` on the enum) and `dump-persist.sh` (parses the arms) |
| `AppState → name` mapping | `app_state_name()` in `pico-core::fsm`, co-located with `discriminant()` | bin's `AppStateLabel::from_discriminant`, `dump-persist.sh` |
| `INIT_STAGE_* → name` mapping | `init_stage_name()` in `pico-core::reset` | same |
| `/health` JSON shape | derived `Serialize` on `Snapshot` | one struct = one shape; no parallel response type |

Failure modes here are caught at:

- **Compile time** for `ResetReason::name` (exhaustive match) and any
  type-level use of `Snapshot` / `ResetReason` (a removed variant or
  field is a hard error in callers).
- **`cargo test`** for the round-trip locks
  (`snapshot_json_contains_every_expected_field` in
  `pico-core/src/persist_data.rs`,
  `reset_reason_name_round_trips_every_variant` in `reset.rs`,
  `every_app_state_variant_has_a_name` in `fsm.rs`).
- **Script run** for the regex parsers in `dump-persist.sh`, which abort
  with a "parse out of sync" message rather than mis-decode silently.

## 4. Surfaces still in parallel

These have no compile-time enforcement that the two sides agree. They're
ordered from cheapest convention to silently riskiest.

### 4.1 `MAGIC` ↔ `PersistRegion` layout

- `MAGIC` is at [persist.rs:64](../crates/anova-oven-pico/src/persist.rs#L64).
- `PersistRegion` is at [persist.rs:93](../crates/anova-oven-pico/src/persist.rs#L93).

**Convention:** bump `MAGIC` whenever `PersistRegion`'s layout
(field set, order, sizes, or `RING_SIZE`/`MSG_BUF_SIZE`) changes. The
comment above `MAGIC` documents the history.

**Failure if forgotten:** old in-RAM data from before the firmware
update keeps passing the magic check after the layout has changed.
`init_at_boot()` then reads garbage offsets into the new layout's
fields. This *has* happened in the codebase's history (see the
"silently rotted" reference in the `dump-persist.sh` header).

**Recovery:** physically power-cycle the board (cold-boot zeroes the
region; in-RAM persistence survives only soft resets). Updating `MAGIC`
prospectively makes future ports clean.

### 4.2 `RING_ENTRY_WORDS = 6` ↔ field count of `RingEntry`

- `RingEntry` is at [persist.rs:74](../crates/anova-oven-pico/src/persist.rs#L74).
- `RING_ENTRY_WORDS` is at [persist.rs:83](../crates/anova-oven-pico/src/persist.rs#L83).

**Caught at:** `dump-persist.sh` run time. The script parses
`RingEntry`'s u32-field count out of the source and asserts it equals
the constant; if not, it aborts with a layout-out-of-sync error
*before* attempting any decode. See lines around the `RING_FIELD_COUNT`
check in the script.

**Failure if `dump-persist.sh` isn't run:** ring decode silently
mis-attributes fields. The firmware itself still works because
`ring_append` / `ring_read` operate on field accessors, not on a u32
index.

### 4.3 `RingEntry` (bin, all-u32 MMIO) ↔ `ResetHistoryEntry` (pico-core, typed)

- `RingEntry` is at [persist.rs:74](../crates/anova-oven-pico/src/persist.rs#L74).
- `ResetHistoryEntry` is at [persist_data.rs:44](../crates/anova-oven-pico-core/src/persist_data.rs#L44).

The two structs must agree on field set and order. They differ only on
field types: `RingEntry` stores everything as `u32` (so the panic-time
writes are atomic single-word operations); `ResetHistoryEntry` is the
typed/decoded view served by `/health` and used in `info!` logs. The
mapping happens inside `ring_read` and `ring_append`.

**Compile-side: partially caught.**

- Adding a field to `ResetHistoryEntry` and trying to populate it in
  `ring_read` forces a read of the corresponding `RingEntry` field — if
  it doesn't exist, that's a compile error.
- Adding a field to `RingEntry` *without* adding to `ResetHistoryEntry`
  is **silent**: the new u32 just sits in the ring, unread, and never
  surfaces in `/health` or `dump-persist.sh` output.

### 4.4 `PersistRegion` fields ↔ `Snapshot` fields

Same shape as §4.3, one layer up.

- `PersistRegion` is at [persist.rs:93](../crates/anova-oven-pico/src/persist.rs#L93).
- `Snapshot` is at [persist_data.rs:104](../crates/anova-oven-pico-core/src/persist_data.rs#L104).

Most of `PersistRegion`'s fields have 1:1 counterparts on `Snapshot`
(e.g. `PersistRegion.last_free_heap` → `Snapshot.last_free_heap`); the
heartbeats are grouped (`api_heartbeat`/`display_heartbeat`/
`watchdog_heartbeat` → `Snapshot.heartbeats: Heartbeats`); a few are
derived (`Snapshot.message_is_new` is computed from `panic_count` vs
`last_displayed_panic_count`); and `Snapshot.uptime_secs` is taken
from `Instant::now()` at read time, not from any persisted field.

**Compile-side: partially caught — same direction as §4.3.**

- `Snapshot` grows → `read_live` won't compile without a source for the
  new field. Forces the author to choose where the value comes from.
- `PersistRegion` grows → no compile error if you forget to expose the
  new field via `Snapshot`. The persistence happens, but `/health`
  silently lacks it.

This is the most likely-to-bite drift surface today. The §6 checklist
exists specifically to flag it during new-field work.

## 5. The structural cause

`PersistRegion` and `RingEntry` are `#[repr(C)]` MMIO views with all-`u32`
fields. That layout is load-bearing for two reasons:

- **Atomic writes from any context.** On Cortex-M0+, a `u32` aligned
  write is a single instruction. The `#[panic_handler]` and HardFault
  handler bump `panic_count` and write `msg_len` without taking a
  critical section, relying on word-atomicity. A bool or enum field
  would break that guarantee or require a CS.
- **`dump-persist.sh`'s decode.** The script reads the region as a
  flat word array from `probe-rs read b32`, then interprets word `i`
  by name based on the parsed struct order. Mixed-size fields would
  require it to know each field's byte offset rather than just word
  index.

`Snapshot` and `ResetHistoryEntry` want the opposite: typed enums,
booleans, `heapless::String`. That's what `/health` exposes and what
the bin's logging consumes.

A macro could in principle generate both views from one definition, but
the divergence between MMIO storage (u32 only, repr(C), volatile reads
in unsafe blocks) and decoded view (typed, `Serialize`, `Clone`) means
the macro would be doing real work, not just rename — and would itself
become a thing to maintain. The current parallel-structs approach is
the lowest-complexity option that keeps the MMIO layer chip-specific
and the decoded layer host-testable.

## 6. Adding a new persisted field — checklist

When adding a new u32-storable breadcrumb to the persist region:

1. **`pico-core::persist_data::Snapshot`** — add a `pub` field with the
   final typed shape (u32 / bool / `&'static str` / nested struct).
2. **`pico-core::persist_data::tests`** — extend `sentinel_snapshot()`
   with a distinct sentinel value for the new field, and add a
   `("field_name", expected.into())` line to the `expectations`
   array. Bump the `obj.len()` count assertion.
3. **`crates/anova-oven-pico/src/persist.rs::PersistRegion`** — add the
   corresponding `u32` field. Pick its position carefully (appending
   keeps the change additive; reordering forces a `MAGIC` bump).
4. **`MAGIC`** — bump if you reordered, changed types, or changed
   `MSG_BUF_SIZE`/`RING_SIZE`. Appending a single u32 at the end of
   `PersistRegion` (before `msg_len`/`msg_buf`) is the only change that
   *might* be safe without a magic bump, but bumping is cheap and you
   pay a power-cycle once.
5. **`zero_region()`** — add a `write_volatile` for the new field if
   you want a guaranteed initial value.
6. **`read_live()`** — read the field and populate the new `Snapshot`
   slot. (Compile error reminds you if you forget the populate.)
7. **Writer** — wherever the field gets updated during the run: add a
   `pub fn record_<field>(...)` if external tasks own writes, or
   update `init_at_boot` if it's a boot-time derivation. The existing
   `bump_*_heartbeat` / `record_*` functions are the pattern.
8. **`init_at_boot()`** — if this field is a "live breadcrumb" cleared
   on boot (like `network_up`, `last_free_heap`, `last_api_fail_count`
   today), add it to the "reset per-run breadcrumbs" block.
9. **If the field belongs in `RingEntry`** — also add it to
   `ResetHistoryEntry` in pico-core, and update `ring_append` and
   `ring_read` to copy it. Bump `RING_ENTRY_WORDS`. The persist_data
   round-trip test will need a new field on the ring-entry sentinel.
10. **Verify the script** — run `scripts/dump-persist.sh` against a
    built ELF. If the script's parsers can't find the new field's
    serialization arm or its name, it aborts loudly. (No probe-rs
    hardware needed for the parse phase — it fails before the read.)
11. **Run `cargo test --workspace --all-features`** — the round-trip
    test will fail on the `obj.len()` assertion until step 2's count
    bump matches reality, forcing acknowledgment of the new schema
    shape.

## 7. Existing drift mitigations — summary

| Layer | Mitigation |
|---|---|
| Compile | Exhaustive matches on enums (`ResetReason::name`). Type-level use of canonical types in `read_live`/`build_response` paths. The dropped `HealthResponse` struct means `/health` has no parallel-struct drift surface at all. |
| `cargo test` | Round-trip JSON test in `persist_data` locks the schema shape (including a top-level-key count assertion). Round-trip name tests in `reset` and `fsm` lock the enum/discriminant ↔ name mappings. |
| Script-run | `dump-persist.sh` parses constants, struct layouts, and name tables out of Rust source; aborts on parse failure. Asserts `RING_ENTRY_WORDS` matches the parsed `RingEntry` field count. |
| Convention | `MAGIC` bump on layout change; checklist above for adding fields. |

## 8. Future hardening — optional

Not implemented today. Listed in case the failure modes in §4 ever bite.

- **`const _: () = assert!(size_of::<PersistRegion>() == EXPECTED);`** next
  to `MAGIC`. Catches any layout-size change at compile time — i.e.
  adding or removing fields without a `MAGIC` bump. Doesn't catch
  reordering that preserves total size. Cheap.
- **`const _: () = assert!(size_of::<RingEntry>() == RING_ENTRY_WORDS * 4);`**
  Same idea for the ring entry; catches the `RING_ENTRY_WORDS`/`RingEntry`
  drift in §4.2 at compile time instead of script-run time.
- **A `derive(MirrorFrom<RingEntry>)`-style macro** that generates
  `ResetHistoryEntry` from `RingEntry` (or vice versa). Would close
  §4.3 and §4.4 entirely but adds a macro-maintenance surface and a
  build-time cost. Probably not worth it unless persisted breadcrumbs
  start changing often.
