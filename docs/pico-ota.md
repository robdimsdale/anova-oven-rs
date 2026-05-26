# Pico W OTA Updates — Feasibility & Implementation Brief

Status: **Investigation only. No code written, nothing committed.**
Date: 2026-05-26 (updated from 2026-05-15 — see §11 changelog)
Scope: `crates/anova-oven-pico` (Raspberry Pi Pico W / RP2040)
Related: [`docs/pico-transport-security.md`](pico-transport-security.md) —
TLS/Noise options for the Pico ↔ server link. Read it for the channel-security
side; this doc covers the firmware-update side.

This document captures everything needed to (a) decide whether to do OTA, and
(b) execute it if we decide yes. It is self-contained — a future agent or human
should not need to re-derive the facts below.

---

## 1. TL;DR

- **OTA is feasible** on the current RP2040 Pico W target.
- **`embassy-boot` (via `embassy-boot-rp`) is the right tool** and supports this board.
- **Flash space is not a constraint**: the firmware now uses ~542 KB of 2 MB
  (up from 511 KB after picoserve `/health` landed — see §3), leaving room for a
  full A/B dual-bank layout (two ~960 KB banks + bootloader + state) with ~418 KB
  per-bank headroom.
- The real work is plumbing, not space: a bootloader binary, a partitioned linker
  layout, the RP2040 execute-in-place (XIP) flash-write path, signed image delivery
  (push *or* pull — see §5a), and aligning embassy crate versions.
- **Delivery direction is now a real choice**, not a foregone conclusion: with
  picoserve already serving `/health` on port 80, adding a `POST /firmware` route
  is a smaller code delta than a polled pull client. Both have viable signing
  stories. See §5a for the tradeoff analysis.
- Rough effort: **~1–2 weeks** for robust, signed, rollback-capable OTA, most of it
  in the delivery/server side and on-hardware testing of the flash-write path.
- This closes `docs/pico-review.md` finding **5.4 [H] "No OTA story"**.

---

## 2. Established facts (measured, not assumed)

| Item | Value | Source |
|---|---|---|
| Board | Raspberry Pi Pico W — **RP2040**, Cortex-M0+ | `Cargo.toml` (`embassy-rp` feat `rp2040`), `.cargo/config.toml` (`--chip RP2040`), `rust-toolchain.toml` (`thumbv6m-none-eabi`) |
| Flash | 2 MB QSPI, execute-in-place (XIP) | `memory.x` |
| Flash layout today | 256 B `BOOT2` @ `0x10000000`, then `FLASH` `ORIGIN=0x10000100 LENGTH=2048K-0x100` | `crates/anova-oven-pico/memory.x` |
| RAM | 264 KB @ `0x20000000` | `memory.x` |
| **Actual flash footprint** | `text 553788 + data 752 ≈ 542 KB` (~27% of 2 MB) | `arm-none-eabi-size` on `target/thumbv6m-none-eabi/release/anova-oven-pico` (measured 2026-05-26) |
| **Actual static RAM** | `data 752 + bss 130172 ≈ 128 KB`, plus 32 KB heap | same |
| cyw43 Wi-Fi blob | `firmware/43439A0.bin` ~225 KB (231,077 B), `include_bytes!`'d into the image | `src/main.rs` |
| Bootloader today | **None.** Single monolithic image. Flash via probe-rs/SWD only | `.cargo/config.toml`, `build.rs` |
| Networking (outbound) | `embassy-net` DHCPv4, **plain HTTP only (no TLS)** via `reqwless` to a local server | `Cargo.toml`, code |
| Networking (**inbound, new**) | `picoserve 0.18` listens on port 80, serves read-only `/health` → live persist snapshot as JSON. One in-flight conn (pool=1), 1 KB TCP buffers, 2 KB HTTP buffer, `close_connection_after_response()` | `crates/anova-oven-pico/src/health.rs`, `main.rs:342` |
| Workspace layout | Pure logic extracted to `crates/anova-oven-pico-core` (host-testable). New `crates/anova-oven-server` (axum, tokio) — Anova cloud relay, not yet an OTA server | `crates/*/Cargo.toml` |

Embassy ecosystem versions currently pinned (`crates/anova-oven-pico/Cargo.toml`):

```
embassy-executor 0.10.0   embassy-rp 0.10.0   embassy-net 0.9.0
embassy-sync 0.8           embassy-time 0.5    embassy-futures 0.1
cyw43 0.7.0                cyw43-pio 0.10.0    reqwless 0.14
picoserve 0.18 (NEW — inbound HTTP server for /health)
```

Note: `anova-oven-pico` is intentionally **not** a workspace member (avoids
thumbv6m/host arch conflicts). It builds standalone.

---

## 3. Why embassy-boot, and what it requires

`embassy-boot` does power-fail-safe A/B firmware updates with trial boots and
automatic rollback. It ships an RP2040 HAL crate: **`embassy-boot-rp`**.

Four flash partitions (all must be page/sector-aligned; RP2040 = 256 B write
page, 4 KB erase sector → align partitions to 4 KB):

- **BOOTLOADER** — small (~8 KB min, ~24 KB with defmt logging). Runs first,
  performs the swap, enforces rollback.
- **ACTIVE** — the running application.
- **DFU** — staging bank the *app* writes the downloaded image into. Must be
  **≥ ACTIVE size + 1 page**.
- **STATE** — swap/rollback bookkeeping (records "update ready", "trial",
  "confirmed").

**ed25519 image signature verification is mandatory, not optional**
(`ed25519-dalek` or `ed25519-salty` feature). This holds **regardless of
whether the transport is encrypted** — see the box below.

Docs reviewed: https://docs.embassy.dev/embassy-boot/0.7.0/default/index.html

### Why signing is mandatory *even with* TLS/Noise on the channel

Transport security (TLS 1.3-PSK or Noise — see
[`docs/pico-transport-security.md`](pico-transport-security.md)) and image
signing protect **different things** and are **not substitutes**:

- Transport encryption protects the image **in flight, on the wire, for the
  duration of the connection**. The moment the bytes leave the socket, the
  guarantee is gone.
- The downloaded image then **sits unauthenticated in the DFU partition** —
  potentially across reboots, power loss, and the bootloader swap — before it
  is executed. TLS says nothing about whether what's in DFU is what we intended
  to ship.
- Signing is an **end-to-end, at-rest** guarantee: the bootloader verifies the
  ed25519 signature of the image in DFU *immediately before* committing/booting
  it, so a corrupted, truncated, or tampered staged image is rejected no matter
  how it got there.
- The trust roots are independent and both must hold: a compromised/misconfigured
  server, a bug in the update path, flash bit-rot, or a maintenance person with
  SWD access can all place a bad image in DFU **without ever touching the
  network**. A TLS-only design has no defense there; signing does.

Bottom line: TLS/Noise hardens the *channel*; signing authenticates the
*payload that will actually run on a device controlling a heating appliance*.
Keep both. Never drop signing on the grounds that "the link is encrypted now."

---

## 4. Proposed flash layout (RP2040, 2 MB)

The current image is **542 KB** (up from 511 KB after the picoserve `/health`
endpoint landed), so each bank only needs ~960 KB and we still have slack.
Suggested layout (4 KB-aligned, indicative — finalize during impl):

```
0x10000000  +-----------------------------+
            | BOOT2 (256 B)               |  RP2040 stage-2 (unchanged)
            +-----------------------------+
            | BOOTLOADER  (~32 KB)        |  embassy-boot-rp binary
            +-----------------------------+
            | ACTIVE      (~960 KB)       |  app bank A (current 542 KB fits, 418 KB free)
            +-----------------------------+
            | DFU         (~960 KB)       |  app bank B / download staging (same headroom)
            +-----------------------------+
            | STATE       (~16 KB)        |  swap state + (optional) signature
0x10200000  +-----------------------------+  (2 MB end)
```

**Bank-fit headroom check** (the user explicitly asked for this):

| | Bytes | KB | % of 960 KB bank |
|---|---:|---:|---:|
| Current app image (text+data) | 554,540 | 542 | 56% |
| Per-bank headroom remaining | ~428,000 | ~418 | 44% |
| Headroom *after* doubling the app (worst-case growth) | ~278,000 | ~272 | 28% |

Both ACTIVE and DFU hold *identical* sized partitions, so if the current image
fits in ACTIVE it fits in DFU by construction. The question is rate of growth:
the image went 511 → 542 KB (+31 KB, +6%) in roughly two weeks of feature work
(picoserve + new health code + alloc-free JSON path). At that rate, ~418 KB
headroom is roughly **6+ months of feature growth** before the bank size needs
revisiting — comfortable, but not infinite. Add a CI size gate (also flagged in
`pico-review.md` 5.2) before this becomes a surprise.

The 225 KB cyw43 blob stays embedded in the app image (it just rides along in
`.rodata`). Externalizing it is **not** required — earlier analysis that called
this a blocker was wrong. It only inflates each bank by ~225 KB, which we can
easily afford.

---

## 5. Work breakdown to execute

1. **Bootloader binary**: a separate crate/bin built with `embassy-boot-rp`,
   with its own `memory.x` placing it in the BOOTLOADER region.
2. **App `memory.x` rework**: split `FLASH` into ACTIVE/DFU/STATE, 4 KB-aligned;
   app linked at the ACTIVE origin; expose partition addresses to the firmware
   updater.
3. **RP2040 XIP flash-write path** (the trickiest correctness area): the chip
   executes XIP from the same QSPI flash it must erase/write. `embassy-rp`'s
   flash driver handles this by running the critical routine from RAM and
   gating interrupts; must be exercised and validated on real hardware, not
   just in theory. Single-core in practice, so no second-core pause concerns.
4. **OTA delivery**: add a path that streams the new image into DFU via
   embassy-boot's `FirmwareUpdater` (`write_firmware(offset, data)` can be done
   incrementally while other tasks run), marks the update, and resets.
   Bootloader swaps on next boot. **`FirmwareUpdater` does not handle delivery**
   — moving the bytes onto the device is entirely the application's
   responsibility. **Two viable directions** — see §5a for the tradeoff:
   - **Pull (device is HTTP client):** device polls the server via existing
     `reqwless`, downloads on update-available. `embedded-update` crate offers
     a ready-made pull protocol (`UpdateService` server / `FirmwareDevice`
     device).
   - **Push (device is HTTP server):** server `POST`s the image to a new
     `/firmware` route on the *existing* picoserve instance. Smaller code
     delta now that picoserve is already running.
4b. **Trial-boot / `mark_booted()` discipline** (operationally critical, easy to
   get wrong): after the bootloader swaps in the new image, **the new firmware
   must call `mark_booted()` itself**, otherwise the bootloader automatically
   reverts to the previous image on the next reset. Implications:
   - The panic handler **must allow the device to reset** (so a crashing new
     image actually triggers rollback rather than hanging).
   - Call `mark_booted()` **only after** the new firmware has verified it is
     actually healthy — e.g. Wi-Fi associated and server reachable — **not**
     immediately at startup. Marking success prematurely defeats rollback.
   - The swap itself is power-fail-safe (backward copy with an atomic progress
     index; resumes mid-swap after power loss), so the risk is logical
     (bad-but-running image marking itself good), not torn flash.
5. **Signing**: enable ed25519 verification; add a build/release step that signs
   images; provision the public key into the bootloader.
6. **Version metadata**: embed build version/SHA, surface it (e.g. via existing
   API/persist region) so the server knows whether to push an update. The
   common embedded convention is to derive this from `CARGO_PKG_VERSION` (or an
   overridable `REVISION` env var) at build time.
7. **Crate version alignment**: pick `embassy-boot` / `embassy-boot-rp` releases
   compatible with the pinned `embassy-rp 0.10` / `embassy-sync 0.8` /
   `embassy-time 0.5` / `embassy-executor 0.10`. **Most likely source of
   integration friction** — mismatched embassy versions.
8. **Provisioning/flashing workflow**: initial flash becomes "bootloader + app
   at their offsets" (probe-rs with adjusted addresses, or a combined UF2),
   slightly more involved than today's single `probe-rs run`. Keep
   USB-BOOTSEL/UF2 as a physical recovery path.
9. **Server side**: endpoint to serve signed images + version negotiation
   (out of scope for the pico crate but required end-to-end).

---

## 5a. Delivery direction: device-pull vs device-push

When this brief was first written, the device had no inbound HTTP server, so
"pull" was the only option that didn't require new infrastructure. **That is no
longer true.** picoserve is already in the image, already bound to port 80,
already running through DHCP-up gating in `main.rs`. Adding a second route
(`POST /firmware`) is a strictly smaller code change than adding a polling
client. The choice now turns on operational properties, not infrastructure cost.

### Side-by-side

| Concern | Pull (device → server) | Push (server → device) |
|---|---|---|
| **New device code** | HTTP client polling loop, manifest parsing, version check, chunked GET, retry/backoff | One picoserve route handler that streams body → `FirmwareUpdater::write_firmware` |
| **Reuses existing infra** | `reqwless` (already used to talk to the API) | picoserve (already serving `/health`) |
| **Trigger latency** | Bounded by poll interval (idle traffic on a normal day) | Immediate — server decides when to push |
| **Idle network cost** | Every poll wakes Wi-Fi for a "no update" response | Zero — connection only opens when an update exists |
| **Server must know device address** | No — device dials out | **Yes** — server needs the device's DHCP-assigned IP. Either device registers on boot, or use mDNS, or static DHCP lease |
| **Inbound auth required** | No (response body is self-authenticated by signature) | **Yes** — anyone on the LAN could `POST /firmware` otherwise. But: **image signing already gates execution**, so an unauth POST cannot install a bad image, only DoS the DFU bank |
| **NAT/firewall friendliness** | Works through any outbound NAT — relevant if the update server ever moves off-LAN | Server and device must be on the same L2/L3 segment (or have routable inbound) — *currently true*, future-uncertain |
| **Failure-mode locality** | Device hangs if server is down at poll time, but device controls retry | Server retries until device ACKs — but device that misses a push window simply doesn't update until next push |
| **Standard library / pattern** | `embedded-update` crate exists, drogue blog covers the pattern | No off-the-shelf "receive-firmware" crate for picoserve; handler is hand-rolled but small (~50–100 lines around `FirmwareUpdater`) |
| **Resource footprint on device** | `reqwless` already linked. Extra static state for poll task + manifest buffer. | picoserve already linked. May need to raise `TCP_RX_BUF_LEN` (currently 1024) and increase `WEB_TASK_POOL_SIZE` from 1 if `/health` and OTA must coexist concurrently |
| **Power profile** | Periodic radio wake-ups even with no update | Radio stays in whatever state Wi-Fi association keeps it in; no extra polling |
| **Test/dev ergonomics** | `curl` mocking the update server | `curl --data-binary @firmware.bin http://<pico-ip>/firmware` — closer to existing `/health` debug ergonomics |
| **Multi-device rollout (future)** | Server is passive; devices stagger naturally | Server controls cadence — easier to canary, easier to thundering-herd if wrong |

### What changed in the calculus

The original brief implicitly assumed pull because that was the only direction
without new server-side infrastructure on the device. The `/health` endpoint
removes that asymmetry:

- **Streaming write fits picoserve's model.** picoserve's handler can read the
  request body in chunks via `embedded-io-async`, hand each chunk to
  `FirmwareUpdater::write_firmware(offset, chunk)`, and never buffer the full
  image. The bottleneck is flash sector erase (~50 ms per 4 KB on RP2040), not
  RAM.
- **Auth concerns largely collapse into signing.** The original "but the server
  needs to authenticate inbound POSTs" objection is materially weaker once you
  accept §3's premise that **signing is mandatory regardless of transport**.
  An unsigned/wrong-key image POSTed by a hostile LAN actor cannot boot. The
  remaining risk is DoS-by-overwriting-DFU, which is real but bounded
  (rollback to ACTIVE still works).
- **`/health` already establishes the inbound-HTTP-as-debug pattern.**
  `POST /firmware` reads naturally as "the write half of the same debug
  surface" — same auth model (none on the wire, signing at-rest), same
  port, same task isolation rationale.

### What hasn't changed

- **Server must still know the device's IP.** Today the firmware logs its IP
  to defmt-rtt but does not register anywhere reachable. Push needs either:
  (a) device announces itself on boot (POST to server with its IP — small,
  ~10 lines via existing `reqwless`), (b) mDNS/`.local` resolution
  (`edge-mdns` exists but is more code), or (c) static DHCP reservation
  (operationally fragile). Option (a) is probably the right minimum.
- **Both directions still need version negotiation.** Pull asks "do you have a
  newer image?" Push needs to know "does this device need this image?" — same
  metadata, different actor.
- **The hard parts are unchanged** regardless of direction: bootloader,
  partition layout, XIP flash-write reliability, signing, `mark_booted()`
  discipline, recovery path. Direction is a UX/operational choice on top of
  those.

### Recommendation

**Lean push** if the deployment stays single-device-on-local-LAN, which it is
today. The code delta is smaller, the trigger latency is better, and the auth
objection is mostly absorbed by mandatory signing. The one new piece of glue
needed is device→server IP registration (~10 lines, reuses `reqwless`).

**Lean pull** if you expect to ever move the update server off-LAN (cloud
hosting, multi-network deployments). Pull is more code on the device today but
more portable tomorrow.

If undecided, **build push first** — it's the smaller increment, validates the
hard parts (signing, swap, rollback) end-to-end, and a future pull client can
hit the same signed-image artifact the push side already produces. The reverse
(starting with pull then adding push) does roughly twice the work.

---

## 6. Alternatives considered (and why embassy-boot wins)

- **embassy-boot-rp** — purpose-built, power-fail-safe, rollback, optional
  signing, integrates with `embassy-rp` flash. **Recommended.**
- **Hand-rolled A/B bootloader** — reimplements exactly what embassy-boot does;
  not worth it here.
- **picotool / UF2 over USB BOOTSEL** — not OTA (needs physical access); keep as
  recovery path only.
- **External storage staging (SD/extra flash)** — unnecessary; 1.5 MB internal
  flash is free.
- Low-level `rp2040-flash` / `embassy-rp` flash — these are the primitives
  embassy-boot already sits on; no reason to use them directly.

---

## 7. Risks / open questions

- **Image signing is mandatory regardless of transport.** It is not a stopgap
  for "no TLS today" — it stays mandatory even after TLS/Noise lands, because
  signing is an end-to-end at-rest guarantee on the staged image while transport
  security only covers the wire (see §3 box and
  [`docs/pico-transport-security.md`](pico-transport-security.md)).
- **XIP flash-write reliability** under load / power loss must be validated on
  hardware; this is the highest-risk technical item.
- **embassy-boot version compatibility** with the pinned embassy stack —
  confirm before committing to the approach.
- **Bricking risk during initial rollout**: until a confirmed-good OTA path
  exists, every device still needs a physical recovery route (SWD/BOOTSEL).
- **Premature `mark_booted()`**: a new image that marks itself good before
  confirming health permanently disables rollback for that update. Treat the
  "what counts as healthy" check as a deliberate design decision (see §5.4b).
- **No tests / no CI size gate** (see `pico-review.md` 5.1/5.2) — OTA increases
  the cost of a bad image; consider adding a size/regression gate alongside.

---

## 8. Pico 2 W / RP2350 note

This project targets **RP2040 only**; there is no RP2350 code path. If migrating
to Pico 2 W later: `embassy-rp` has gained `rp235x` support, but
`embassy-boot-rp`'s RP2350 story is **less mature** and RP2350 has a different
boot flow (signed boot blocks, optional secure boot/OTP). Treat RP2350 OTA as a
**separate investigation**; do not assume embassy-boot-rp supports it as cleanly.
For the current RP2040 target, support is solid.

---

## 9. Recommendation

Proceed if OTA is a product priority. It is technically sound and space is not a
constraint — the current 542 KB image fits in a 960 KB bank with ~418 KB
headroom (see §4 bank-fit table). Sequence: (1) confirm
embassy-boot/embassy-boot-rp version compatibility with the pinned embassy 0.10
stack, (2) stand up bootloader + partitioned layout and validate the XIP
flash-write + rollback path on hardware with a trivial app, (3) **default to
push delivery** (smaller delta on top of the existing picoserve `/health`
infra — see §5a) with signed images and device→server IP registration, (4)
integrate into the real firmware, (5) keep SWD/BOOTSEL recovery throughout
rollout. If product direction changes to put the update server off-LAN, swap
step (3) for pull via `embedded-update`.

---

## 10. Resources

- **embassy-boot docs** (reviewed for this brief):
  https://docs.embassy.dev/embassy-boot/0.7.0/default/index.html
- **Drogue blog — "Firmware updates" part 1** (bootloader/partition/swap
  architecture, trial-boot & `mark_booted()` discipline, power-fail-safe swap):
  https://blog.drogue.io/firmware-updates-part-1/
  - Part 2 (delivery transports, `embedded-update` pull protocol, version via
    `CARGO_PKG_VERSION`/`REVISION`) follows from part 1:
    https://blog.drogue.io/firmware-updates-part-2/
  - ⚠️ **Caveat — these posts are dated.** At the time of writing they list
    firmware signing as unimplemented "future work" and RP2040 as a wishlist
    item with only nRF52/STM32 supported. Both have since landed
    (`embassy-boot-rp` exists; ed25519 verification is available via
    `ed25519-dalek`/`ed25519-salty` features). Use the posts for the
    *architecture and operational discipline*, not for current
    platform/feature support.
- `embedded-tls` (TLS 1.3-only client used by `reqwless`):
  https://github.com/drogue-iot/embedded-tls — see
  [`docs/pico-transport-security.md`](pico-transport-security.md).
- Related in-repo: [`docs/pico-transport-security.md`](pico-transport-security.md),
  `docs/pico-review.md` (findings 5.1/5.2/5.3/5.4).

---

## 11. Changelog

- **2026-05-26** — Refreshed against current codebase. Re-measured image
  (511 → 542 KB after picoserve `/health` landed; static RAM 91 → 128 KB).
  Noted new `crates/anova-oven-pico-core` (host-testable logic split) and
  `crates/anova-oven-server` (axum). Added §5a tradeoff analysis for
  device-pull vs device-push delivery now that an inbound HTTP server
  (picoserve 0.18) exists in the image. Added explicit bank-fit headroom
  table in §4. Recommendation in §9 now defaults to push-first.
- **2026-05-15** — Initial brief.
