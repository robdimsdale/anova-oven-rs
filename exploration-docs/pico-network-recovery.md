# Pico W network recovery — debugging notes

Context for future debugging of the Anova Pico firmware's behavior when the
upstream HTTP server (`anova-oven-server`) becomes unreachable, then comes
back. This is a record of what we tried, what worked, what didn't, and the
hard constraints we discovered along the way.

## Original symptom

- Pico W is happily polling the server (`/status` every 1s, `/current-cook`
  every 10s, etc.) over plain HTTP on the LAN.
- The Linux box hosting `anova-oven-server` is restarted (process bounces;
  the host stays up at the same IP).
- After the bounce, the Pico logs `GET /status: connection failed` on every
  poll, indefinitely.
- `curl <api-server-host:api-server-port>/status` from any other machine still works fine.
- A power cycle of the Pico fixes it; nothing else does.

The Pico's `connection failed` is `client.request(...)` returning `Err`
*before* sending — i.e., a TCP-connect failure, not an HTTP error. Since the
URL is a literal IP, no DNS is involved.

## What is actually broken (best current understanding)

A `defmt` log captured during a stuck state revealed two key things:

1. **`failed to push rxd packet to the channel.`** from
   `cyw43-0.7.0/src/runner.rs:881`. The cyw43 driver is trying to push a
   received Wi-Fi frame into its channel to `embassy_net`'s `NetDriver`, and
   the channel is **full**. The packet is dropped.
2. cyw43 0.7.0 hardcodes that channel as `ch::State<MTU, 4, 4>` in
   `cyw43-0.7.0/src/lib.rs:125` — only **4 RX and 4 TX buffers**, not
   configurable. There is no public API to change it without forking.

The plausible chain of events:

- Server bounce → Pico's in-flight TCP connections die.
- Pico keeps polling at 1Hz. Each poll opens a fresh TCP socket.
- Each socket sits in `SYN_SENT` until `with_timeout` (3s) gives up. During
  that 3s, smoltcp is retransmitting SYNs and the underlying state lingers
  for some additional time after the future is dropped.
- New polls keep arriving every 1s while old ones are still draining.
- When the server comes back, the resulting burst of late SYN-ACKs / RSTs
  arriving on the Pico finds the cyw43 RX channel full, and packets get
  dropped — including the SYN-ACKs the new connections are waiting for.
- System wedges. Power cycle is the only thing that clears smoltcp socket
  state, the cyw43 channel, and any stale ARP entries simultaneously.

This is consistent with all observed evidence but is not 100% confirmed. The
diagnostic edits we left in place (see below) should print the actual
`reqwless::Error` variant on the next reproduction, which would confirm or
refute it.

## What we tried

### 1. Wi-Fi link recovery via `cyw43::Control` — **dead end**

Attempted to detect a "stuck" state and call `control.leave().await` followed
by `control.join(...)` to reset the Wi-Fi association.

- With `leave + join`: link layer recovered cleanly. Log showed
  `Disassociated` → `Wi-Fi rejoined` → `Recovered IP address: 10.0.1.34/24`.
  But TCP connections **continued to fail immediately afterward**, proving
  the issue is below TCP-connect, not at the link layer. After running for a
  bit, the cyw43 driver corrupted itself and panicked:
  ```
  unexpected ethernet type 0x0038, expected Broadcom ether type 0x886c
  packet too short, len=0
  panicked at 'assertion failed: addr % 4 == 0' (cyw43-0.7.0/src/spi.rs:272)
  ```
- With `join` alone (no `leave` first): immediate panic on the very first
  recovery attempt:
  ```
  panicked at 'IOCTL error -5' (cyw43-0.7.0/src/runner.rs:714)
  ```

**Conclusion: cyw43 0.7.0's `Control` API cannot be used to safely
re-associate at runtime.** Both code paths crash the driver. Do not
re-introduce this. If you want runtime Wi-Fi recovery, the only path is
upgrading cyw43 (currently 0.7.0 is the latest published version) or forking
it. A soft-reset via `SCB::sys_reset()` would also work but the user
explicitly does not want to reset the whole Pico.

The recovery code has been **fully removed** from `main.rs`. The
`recover_link` helper, the `LINK_RECOVERY_*` constants, and the `last_recovery_at`
tracking are all gone. The `with_timeout` import was also dropped.

### 2. `PowerManagementMode::PowerSave` → `None` — **made things worse**

Theory was that PowerSave was letting the cyw43 chip sleep aggressively and
miss a deauth, leaving it in a ghost-associated state. Switching to `None`
should have made the link more responsive.

In practice this contributed to the SPI corruption panics above — likely by
changing the rate of SPI traffic in a way that exposes a cyw43 bug.

**Reverted.** PowerSave is restored. Do not change it without first solving
the cyw43 0.7.0 SPI stability issue.

### 3. `StackResources<5>` → `<8>` — **kept**

Bumped smoltcp's socket pool from 5 to 8 (in `embassy_net::new` →
`StackResources` static at the bottom of `main()`). This was a free safety
margin in case stuck sockets were exhausting the pool. Statically allocated,
no runtime cost beyond a small RAM bump. Does not fix the bug on its own,
but it doesn't hurt either.

### 4. Improved error logging in `api.rs` — **kept**

Each `connection failed` / `send failed` log now includes the actual
`reqwless::Error` variant via `{:?}`. There's also a `debug!` line right
before each request showing `link_up` and `config_up` from
`embassy_net::Stack`. The `defmt` feature on `reqwless` is already enabled in
`Cargo.toml`, so these print readable error names.

This is the most useful thing left from the failed experiments. On the next
reproduction, the log will tell us whether the failure is
`NoSocketAvailable`, a connection-refused, a timeout, or something else, and
whether the link/config layers think they're up at the moment of failure.

### 5. Polling backoff — **kept (current fix)**

The actual fix in flight, in `app_state.rs` and `main.rs`:

- New helper `AppState::next_poll_interval_secs()` returns the next poll
  interval as a function of `server_fail_count`:
  - 0–4 fails: **1s** (`NORMAL_POLL_INTERVAL_SECS`)
  - 5–9 fails: **5s**
  - 10–14 fails: **15s**
  - 15+ fails: **30s** (cap)
- The main loop in `main.rs` schedules `next_poll_at` as
  `Instant::now() + Duration::from_secs(app.next_poll_interval_secs())`
  after each poll, instead of accumulating a fixed `POLL_INTERVAL_SECS`.
- On a successful poll, `server_fail_count` is reset to 0 (existing
  behavior in `poll_status_if_due`), so the cadence snaps back to 1s.

The reasoning: we cannot fix smoltcp/cyw43 state at runtime, but we can stop
making it worse. By backing off polling under failure, we let:

- smoltcp's TCP state machine reach FIN/CLOSED on stuck sockets and free
  them.
- The cyw43 RX channel drain naturally as `embassy_net` catches up.
- Any stale ARP entries time out.

When the server comes back, we will at minimum attempt one connection every
30 seconds, so recovery is automatic — just slower than the user might want.

User-initiated actions (`/start`, `/stop`) are **not** affected by backoff.
They go through `process_pending_api_action` in the main loop, which runs
every loop iteration (every ~50ms), independent of polling. So pressing the
encoder button to start/stop a cook should still feel instant even when
status polling is in deep backoff.

`/current-cook` polling is still gated by `tick % COOK_POLL_INTERVAL == 0`
inside `poll_status_if_due`, and `tick` only advances on poll attempts. So
in deep backoff, `/current-cook` polls slow down proportionally (every 10
poll attempts = every 5 minutes at the 30s tier). That's intentional — if
status is failing, there's no point hammering `/current-cook`.

## What we did **not** do

- **Soft reset** via `cortex_m::peripheral::SCB::sys_reset()` after N
  failures. User explicitly rejected this. It would work, but it's a "throw
  the whole machine away" answer.
- **Upgrade `cyw43`** beyond 0.7.0. 0.7.0 is the latest published as of
  this conversation. A git dependency on `embassy-rs/embassy` main might
  have a newer cyw43 with fixes and possibly configurable buffer counts,
  but that's a much bigger change with its own risks.
- **Fork cyw43** to bump `ch::State<MTU, 4, 4>` to something larger
  (e.g., `<MTU, 16, 16>`). This would directly address the dropped-RX-packet
  issue and might be the right answer eventually, but it's a bigger
  commitment than the user wanted right now.
- **Restructure the API layer** to share a long-lived `TcpClientState`
  rather than creating a fresh one per call. Each function in `api.rs`
  currently does `let client_state = TcpClientState::<1, 1024, 1024>::new()`
  on the stack. Sharing one instance across calls would reduce socket
  churn, but it requires plumbing the state through `AppState` and adds
  borrow-checker friction. Worth considering if backoff doesn't fully solve
  it.

## What works fine and shouldn't be touched

- `PowerManagementMode::PowerSave` (changing it crashes cyw43).
- The per-call `TcpClientState` pattern (works under normal conditions; the
  failure mode is specifically about *recovery*, not steady state).
- The 3-second `with_timeout(...)` wrappers around each API call in
  `app_state.rs` (these are what kept the UI responsive even when the
  server was unreachable, before backoff was added).
- The `cyw43_task` and `net_task` spawn order in `main.rs`.

## How to reproduce the original bug

1. Flash the Pico with the firmware pointing at a known-reachable server.
2. Verify it polls successfully (`/status` requests appear in server logs).
3. Stop the server process. Wait ~30 seconds. Restart the server.
4. Observe the Pico's defmt log.
   - **Expected (with current backoff fix):** repeated `connection failed`
     entries with the cadence visibly slowing as `server_fail_count` climbs;
     eventually one succeeds and the cadence snaps back to 1s. The new
     `link_up=... config_up=...` debug line should help confirm what state
     the stack thinks it's in.
   - **What we used to see:** `connection failed` every second forever,
     until power cycle, with intermittent
     `failed to push rxd packet to the channel.` warnings from cyw43.

If the bug is *not* fixed by backoff, the `reqwless::Error` variant in the
new log lines will tell us what to look at next:

- `NoSocketAvailable` / similar → smoltcp socket pool is genuinely
  exhausted; the right move is sharing `TcpClientState` across calls or
  bumping `StackResources<8>` further.
- `ConnectionRefused` / `ConnectionAborted` → server-side or routing
  problem, not a Pico-side bug.
- `Tcp(...)` with a smoltcp variant → likely the cyw43 RX-drop issue, and
  the right move is forking cyw43 to enlarge `ch::State`'s buffer counts.
- `Dns(...)` → shouldn't happen since we use a literal IP, but would
  indicate accidental DNS lookup somewhere.

## Files of interest

- [crates/anova-oven-pico/src/main.rs](../crates/anova-oven-pico/src/main.rs) —
  network init, `cyw43_task`, `net_task`, main loop, `next_poll_at`
  scheduling.
- [crates/anova-oven-pico/src/app_state.rs](../crates/anova-oven-pico/src/app_state.rs) —
  `poll_status_if_due`, `server_fail_count`, `next_poll_interval_secs`,
  backoff tier constants.
- [crates/anova-oven-pico/src/api.rs](../crates/anova-oven-pico/src/api.rs) —
  HTTP client functions, error logging, link/config debug logs.
- [crates/anova-oven-pico/Cargo.toml](../crates/anova-oven-pico/Cargo.toml) —
  pinned cyw43 0.7.0; reqwless 0.14 with `defmt` feature on.

## Open questions / next steps if backoff isn't enough

1. Confirm the cyw43 RX-drop theory by counting `failed to push rxd packet`
   warnings during a reproduction. If they correlate with the wedge, the
   fix has to be at the cyw43 layer (fork or upgrade).
2. Try a git dependency on embassy-rs main to see if cyw43 has been updated
   with either fixes for the SPI panics or larger / configurable channel
   buffers.
3. If forking cyw43, the minimal change is `ch::State<MTU, 4, 4>` →
   `ch::State<MTU, 16, 16>` in `cyw43-0.7.0/src/lib.rs:125`.
4. Consider sharing one `TcpClientState` across all API calls to reduce
   per-call socket churn. This would require holding it in `AppState` (or
   a static) and is a borrow-checker exercise but probably tractable.
5. If all else fails, revisit soft reset — it's the nuclear option but it
   does work, and a reset that only fires after, say, 60 consecutive
   failures (i.e., a full minute of total inability to talk to the server)
   is a much milder thing than a reset on every blip.
