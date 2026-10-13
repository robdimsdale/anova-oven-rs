# Runtime network recovery: the panel that never comes back

The firmware joins WiFi exactly once, at boot (`main.rs`, the
`WIFI_JOIN_DEADLINE_SECS` block). Nothing re-joins afterwards. If a panel loses
its association mid-run and the chip's own supplicant gives up, the box sits
showing "Server offline" until someone power-cycles it — with the watchdog
healthy, `/health` unreachable, and no breadcrumb recorded.

This has not been observed in months of running. That is consistent with the
mechanism rather than luck (see "Why this is rare" below), but the tail is
unbounded: the consequence isn't degraded service, it's a panel that never
recovers on its own.

This document records what actually recovers a dropped association today, where
the gap is, and a staged plan. Tasks are ordered so each one is independently
shippable and testable.

Adjacent to `docs/pico-server-transport.md` — that one is about the cost of
talking to the server, this one about what happens when the panel can't talk to
anything.

## What recovers a dropped association today

Three layers, and only one of them is missing. Established by reading the
vendored driver sources, not inferred.

**The IP layer self-heals.** `embassy-net-0.9.1/src/lib.rs:900-925` handles link
transitions itself: on link down it resets the DHCP socket and clears the
config; on link up it resets DHCP and re-DISCOVERs. A new lease — even a
different address — needs no host involvement. **DHCP is not the gap.**

**The association is re-established by the chip, not the host.**
`cyw43-0.7.0/src/control.rs:313` (`Control::join`) is one-shot: it installs the
PMK via `SetWsecPmk`, sets `bsscfg:sup_wpa = 1` — which offloads the WPA
supplicant into chip firmware — then fires `Ioctl::SetSsid` and waits for the
join event. Because the credentials *and* the supplicant live in firmware, the
CYW43439 can redo the whole 4-way handshake without ever talking to the host.
This is what recovers the panels in practice.

**The host does nothing, ever.** The driver has no reconnection logic at all. On
`Event::LINK` flag 0, `Event::DEAUTH`, or a WPA3 `Event::AUTH` failure it clears
`join_ok` / `auth_ok` / `key_exchange_ok` and sets `LinkState::Down`
(`cyw43-0.7.0/src/runner.rs:779-785`). Nothing re-arms it. The driver also never
sets any roam iovars — `roam_trigger`, `roam_delta` and `roam_off` appear
nowhere in the crate — so the chip's retry policy is entirely whatever is baked
into `43439A0.bin`.

**That last point is the crux, and it cannot be settled from source.** How
persistently the chip retries before giving up is in the firmware blob. What
*is* certain is that if it ever gives up, `LinkState::Down` is latched host-side
and only another `control.join()` can clear it.

## Where the gap is

`control` is used for the last time inside the boot join loop in `main.rs`. It
stays alive in `main`'s frame for the process lifetime but is never moved into a
task and never touched again. **No code path can call `join()` after bring-up.**

The correct pattern already exists ten lines above it. The comment on
`WIFI_JOIN_DEADLINE_SECS` reasons about precisely this class of bug:

> The WiFi join and DHCP waits used to be unbounded loops; with the watchdog
> feeder running unconditionally, a stall here (e.g. associated to the AP but no
> DHCP lease) hangs the box forever instead of recovering.

That reasoning produced the bring-up deadlines and `reboot_init_timeout()`. It
was never extended past bring-up.

## Why it is nasty when it happens

Every recovery and diagnostic mechanism is disabled in exactly this state:

- **The watchdog cannot catch it.** `watchdog_feeder_task` feeds
  unconditionally, by design. The 8 s watchdog stays healthy throughout.
- **The poller never escalates.** `fail_count` climbs past
  `POLL_BACKOFF_TIER3_FAILS` to a 30 s interval and stays there forever. Each
  attempt burns 5 s on `with_timeout` against a dead stack. No deadline, no
  reboot.
- **`/health` is unreachable** — it is served over the same stack that is down.
  The one endpoint built for diagnosing the panel is offline precisely when it
  is needed.
- **The persist ring records nothing**, because no reset occurred. There is no
  concept of "network died at uptime N".
- **The screen misdirects.** It says "Server offline", so the investigation
  starts on the server, which is fine.

Net result: a panel bricked-on-screen until power-cycled, no evidence trail
afterwards, and an error message pointing at the wrong component.

## Why this is rare

Brief blips don't reach it — the chip handles those. The exposure is the cases
where the chip stops trying or cannot succeed:

- AP down longer than the firmware's retry persistence, then it stops.
- `PSK_SUP` key-exchange errors (`runner.rs:816`) or a WPA3 `AUTH`/reason-16
  failure (`runner.rs:781`) — both explicitly latch link-down and clear flags.
- AP security settings or PSK changed while the panel was associated.
- SSID moves to a channel the firmware won't rescan for.
- A chip-level wedge — an ioctl that never completes, leaving the driver waiting
  forever. Hypothetical: no instance has been observed here, and it is listed
  because it is the one case a reboot fixes and a rejoin does not. (Not to be
  confused with the `firmware-logs` alignment panic noted in the pico
  `Cargo.toml` — that one *panics* the MCU, which soft-resets and therefore
  recovers the network as a side effect. It is also off by default.)

And one thing actively reduces exposure: `PowerManagementMode::None` in
`main.rs`. The radio never sleeps, so it doesn't miss beacons and doesn't
accumulate the spurious beacon-loss deauths a power-saving client does. Months
without a hit is consistent with an always-awake radio plus a firmware
supplicant that covers the common cases.

## The design decision: what a reboot is actually for

The obvious escalation is time-based — network down for N seconds, try a
rejoin, still down, reboot. Reading `JoinError` (`control.rs:23`) gives a better
discriminator than a stopwatch.

A reboot only fixes one failure class: a **wedged chip**, because only a reset
re-runs `control.init()` and re-uploads firmware and CLM. It does nothing about
an absent AP. Worse, rebooting into a 45 s join deadline during a long AP outage
produces a reboot loop that wraps the 8-entry ring (`RING_SIZE`) and destroys
the evidence the reboot was supposed to leave behind.

`join()` says which case it is:

| Result | Meaning | Action |
| --- | --- | --- |
| `Ok(())` | Recovered | Record, resume |
| `Err(NetworkNotFound)` | AP absent | Keep retrying, **never reboot** |
| `Err(AuthenticationFailure)` | PSK / security changed | Log loudly, retry slowly |
| `Err(JoinFailure(status))` | Transient | Retry on backoff |
| `with_timeout` expires | Chip not responding | **Reboot** |

So: **retry forever on a backoff, and reserve the reboot for a join attempt that
never comes back.** That mirrors the existing bring-up idiom (`with_timeout` +
deliberate reset) and removes the reboot-loop and ring-wrap problems entirely.

## Phase 0 — make the failure observable

No behaviour change, zero risk, independently shippable. Worth merging on its
own: if this recurs before the rest lands, it leaves data instead of anecdote.

`persist.rs` — add live fields to `PersistRegion`:

| Field | Purpose |
| --- | --- |
| `link_up` / `config_up` | Last observed stack state |
| `net_loss_count` | Times the network dropped this run |
| `last_net_loss_uptime_secs` | When the most recent drop began |
| `net_down_secs` | Duration of the current or last outage |

Bump `MAGIC` to `v4 = 0xA9B0_C1D5`, adding a line to the existing history
comment block.

`persist_data.rs` — mirror the fields into `Snapshot`. They reach `/health`
automatically through the derived `Serialize`; that drift contract already holds
and needs no new code.

**Writer: `watchdog_feeder_task`.** It already ticks every 2 s and already
records uptime and free heap; it needs the `stack` handle passed in. The
alternative site — `fetch_status`, which already reads both booleans and
discards them at `debug!` (compiled out under the default `DEFMT_LOG=info`) —
only samples when a poll runs, which backs off to 30 s exactly when it matters
most.

Tests: extend the `persist_data` round-trip test for the new fields.

## Phase 1 — pure escalation policy in `pico-core`

The codebase keeps decisions pure and effects in the bin (`fsm`, `view_plan`,
`lcd_plan`). The supervisor follows it, so the escalation logic is host-testable
without hardware.

New `crates/anova-oven-pico-core/src/net_supervisor.rs`:

```rust
pub enum NetAction { Wait, Rejoin, Reboot }

pub struct NetSupervisorState {
    pub down_since: Option<Instant>,
    pub attempts: u32,
    pub last_outcome: Option<JoinOutcome>,
}

pub fn decide(state: &NetSupervisorState, config_up: bool, now: Instant) -> NetAction
```

`JoinOutcome` is a local mirror of the cyw43 `JoinError` cases plus `TimedOut`.
pico-core must not depend on cyw43, so the bin maps one onto the other.

Constants, with the reasoning that picks them:

- `RECOVERY_GRACE_SECS = 60` — do nothing at first. The chip's supplicant
  handles the common cases, and racing it with a host `SetSsid` risks
  interrupting a reassociation already in flight.
- `REJOIN_BACKOFF_SECS` — 5, 15, 30, then 60 steady-state. Bounded, no runaway.
- `JOIN_ATTEMPT_TIMEOUT_SECS = 30` — under `WIFI_JOIN_DEADLINE_SECS` (45),
  because this bounds a single attempt rather than a retry loop. Expiry is the
  reboot trigger.

Tests (host, pure): grace period suppresses action; `NetworkNotFound` never
escalates to `Reboot` however many attempts accumulate; a timeout escalates on
first occurrence; recovery resets the state.

## Phase 2 — the supervisor task

`main.rs` — move `control` into a new `net_supervisor_task(control, stack)`.
`Control<'a>` holds only shared references and a `StateRunner`
(`control.rs:39-44`), so `Control<'static>` moves into an embassy task cleanly.
No `Mutex` is needed, since nothing else uses it after bring-up.

Loop:

1. `stack.wait_config_down().await`. `config_up` is the right predicate: it
   covers both a lost association and a DHCP lease that cannot be renewed.
2. Record the drop (Phase 0 fields).
3. Poll `decide()` on a timer. On `Rejoin`, run
   `with_timeout(JOIN_ATTEMPT_TIMEOUT_SECS, control.join(..))`, map the result
   to `JoinOutcome`, feed it back.
4. On `Reboot`, set the breadcrumb and reset.
5. On recovery, record and return to step 1.

embassy-net re-runs DHCP by itself once the link is back, so this task handles
association only.

## Phase 3 — reset attribution

`reset.rs`:

- `ResetReason::NetworkLost = 7`, plus `from_u32` and `name()` arms.
- `RUNTIME_STAGE_NET_LOST: u32 = 110` — a runtime breadcrumb, deliberately
  *not* an `INIT_STAGE_*`, so "reset during bring-up" stays distinguishable from
  "reset after running fine for hours".
- `classify_reset` gains a branch ahead of the `OtherSoftReset` fallback.

`AppStateLabel::from_discriminant` falls back `app_state_name` →
`init_stage_name` → `"Unknown"`. Add a `runtime_stage_name` step, or 110 shows
as `Unknown` in `/health`.

Tests that will break and must be updated:

- `from_u32_unknown_values_decode_to_unknown` asserts `from_u32(7) == Unknown`;
  becomes 8.
- `reset_reason_name_round_trips_every_variant` has a hardcoded variant array.
- `init_stage_name_covers_known_stages` loops `0..=200` asserting
  `is_init_stage` and `init_stage_name` agree — stays green as long as 110 is
  added to *neither*, which is the intent.

`scripts/dump-persist.sh` regex-parses `reset.rs` for one-arm-per-line
`Self::Variant => "Variant"`. Keep that shape.

## Phase 4 — stop the screen lying

`ApiSnapshot` gains `link_up` / `config_up`, published by the api client. No new
`AppState` variant — `execute_offline` keeps its state and picks the view, which
avoids touching discriminants and persist breadcrumbs.

New `ViewSpec::NetworkLost { down_secs }`. Exhaustive matches to update:

- `fsm.rs` — the enum and `timer_age_secs`
- `view_plan.rs` — graphics layout
- `lcd_plan.rs` — `rows("No Wi-Fi", "Panel offline")`
- `tft_ui.rs` — `palette_for`; AMBER, alongside `ServerOffline`

Result: "No Wi-Fi" when the panel's own link is down, "Server offline" only when
the link is up and the server isn't answering.

## Phase 5 (optional) — error taxonomy on screen

Independent of Phases 0-4; do it whenever. It is what makes the remaining
"Server offline" cases precise.

`ApiError` lives in the bin, so `ApiSnapshot` in pico-core cannot hold it. Move
the enum to `pico-core/src/api.rs` — it is pure data, and that module already
owns pre-flight logic while the transport stays in the bin. Then
`record_fast_poll_failure` stores the variant instead of discarding it, and the
offline screen's second row can show `Http 503` versus `No route` versus
`Bad JSON`.

The distinction matters: an HTTP 503 means the server is *up*. Today that is
indistinguishable from a dead network on the panel, and the typed `ApiError`
that already exists is thrown away at the one point it could be recorded.

## Open item: does `join()` work from a latched link-down state?

Unverified against hardware, and not determinable from the driver — the
behaviour is in `43439A0.bin`. The question is whether `join()` succeeds when
the chip believes it is still associated but the link is down, or whether
`control.leave()` (`control.rs:752`, a `Disassoc` ioctl) must precede it.

Plan: try `join()` alone first, add `leave()` if it fails, and record which was
needed in a comment at the call site.

## Validation

Host tests cover the escalation policy, reset classification, the persist
round-trip and both view planners, via `./scripts/build-and-test.sh`.

Hardware reproduction, easiest first:

1. **AP password change** — exercises the `AuthenticationFailure` path.
2. **Power-cycle the AP** — grace period, then chip-or-host recovery. The
   honest end-to-end case.
3. **MAC-filter or deauth the panel from the router** — targeted, and leaves the
   second panel as a control.
4. **Chip wedge** has no clean trigger. Cover the reboot path with a temporary
   build that forces a join timeout, and verify `NetworkLost` lands in
   `reset_history`.

## Recommendation

Phase 0 alone, merged on its own, converts the next occurrence from anecdote
into data. Phases 1-2 are the actual fix. Phases 3-4 make it legible in
`/health` and on the panel. Phase 5 whenever.

Revisit the grace period if hardware testing shows the chip's supplicant gives
up faster than 60 s — the constant exists to avoid racing it, and the right
value is an empirical question this document cannot answer.
