# Device ↔ server transport: reducing quiescent noise

The firmware polls the server once a second. The question that prompted this
document was whether the server should instead *push* state to devices —
registering them, tracking liveness, deregistering them after inactivity.

Short answer: **push is not worth it; the cheap wins are elsewhere.** This
records why, what long-poll actually involves if it is ever taken up, and the
one subtle bug a naive implementation has.

Independent of `docs/pico-display-refresh.md`. That document is about what the
firmware does with data once it has it; this one is about getting it.

## Motivation: quiescent noise, not cost

The goal driving this is **not** CPU, power or server load. It is that the oven
is idle for the overwhelming majority of the day, and a system that chatters
once a second through all of it is hard to observe. Noise buries signal: a
packet capture, a log tail or a router's connection table should be quiet when
nothing is happening, so that something happening is visible.

That objective is worth stating because it leads somewhere different from a cost
analysis. Under a cost framing, 1 Hz polling on a LAN is cheap and the answer is
"leave it alone." Under a noise framing, the relevant question is what is
actually generating the noise — and the answer turned out to be surprising.

## Where the noise actually comes from

Measured against the code rather than assumed. The two halves have **different
sources**, and conflating them leads to fixing the wrong one.

### Logs: device polling contributes nothing

- **Device.** `.cargo/config.toml` sets `DEFMT_LOG = "info"`. The two per-poll
  log sites in `fetch_status` (`api.rs:128` and `:148`, link/config state and
  the decoded status) are both `debug!` — **compiled out** of the default build.
  A successful poll emits nothing at all; only `warn!` on failure.
- **Server HTTP.** `handle_status` logs `trace!("[http] GET /status")`
  (`processors/http.rs:114`), and the default `EnvFilter` is
  `anova_oven_server=info`. Also filtered out. A `/status` poll produces **no
  log line**.
- **Server upstream.** The same default filter sets
  `anova_oven_server::processors::ws=debug` deliberately, and `ws.rs` logs
  `debug!(cook = %cook, "[ws] state.cook")` (`:406`) plus another `debug!` at
  `:426` inside the frame-handling path. **Every oven-state frame Anova pushes
  produces log lines.**

So the log noise is entirely the *upstream* WebSocket, which Anova drives on its
own schedule regardless of what any device does. **Changing the device poll rate
would not remove a single log line.** If logs are the complaint, the fix is the
`EnvFilter` default or making `ws.rs` log on *change* rather than per frame —
both far cheaper than touching transport, and neither has anything to do with
this document's main subject.

### Traffic: real, and device-driven

The ~95,000 connections/day below are genuine and visible to anything watching
the network. This is the half a transport change addresses.

**Fix the logging question first.** It is minutes of work, it is orthogonal, and
it may be the entire complaint.

## Where the push already is

Only the last hop polls:

```
Anova cloud ──WebSocket push──▶ server ──HTTP poll (1 Hz)──▶ Pico
```

`processors/ws.rs` holds a persistent WebSocket to Anova, refreshes Firebase
tokens, detects half-open sockets and reconnects, feeding oven-state frames into
the state machine. That half is already event-driven. This document is only
about the second arrow.

## What polling costs today

| Endpoint | Interval | Requests/day | Socket buffers |
| --- | --- | --- | --- |
| `/status` | 1 s (`NORMAL_POLL_INTERVAL_SECS`) | 86,400 | 1024 / 1024 |
| `/current-cook` | 10 s | 8,640 | 1024 / 1024 |
| `/recipes` | 3600 s | 24 | 4096 / 4096 |

Each is a **fresh TCP connection**: `api::request()` constructs
`TcpClientState::<1, TX, RX>::new()` per call, so every poll is connect → GET →
response → close. That is roughly **95,000 TCP connections per device per day**,
each around 8–10 packets over WiFi, each a radio transaction through cyw43's
PIO/DMA path.

Failure handling is already reasonable: `API_CALL_TIMEOUT_SECS` (5 s) wraps each
call, `fail_count >= OFFLINE_THRESHOLD` (3) surfaces `ServerOffline`, and
`next_poll_interval_secs` backs off through 5 s / 15 s / 30 s tiers at 5 / 10 /
15 consecutive failures.

**The current design's real virtue** is that it has no persistent state to get
wrong. Every request is independent; any failure self-heals on the next tick.
There is no half-open socket failure mode anywhere in the device.

## Evaluating the push proposal

### What it buys

| | Value |
| --- | --- |
| Fewer TCP handshakes | **Real** — ~95k/day → a handful |
| Lower latency | Marginal — 1 s already, input is local, and the cook timer is extrapolated on-device at 1 Hz |
| Fewer device wakeups | Real, but no power win: mains-powered, and `PowerManagementMode::None` keeps the radio fully awake regardless |
| Server knows who is listening | True, but no feature needs it today |
| Server load | Non-issue either way — three devices is three requests/second |

One genuine win, and it is a cost that is not currently hurting.

### What it costs

**Registration, deregistration, TTLs.** New state to own and a new way to be
wrong: stale registrations pointing at devices that moved, rebooted or died.

**A persistent connection on a microcontroller.** This is the real cost, and
there is direct evidence in this repository of what it takes. `liveness.rs` (191
lines) plus a large part of `ws.rs` exist *because of* the upstream persistent
connection — reconnect storms, staleness detection, "no oven-state frame from
Anova within timeout; reconnecting", token expiry. The comment at `ws.rs:32`
notes that a stale token "makes every reconnect attempt fail forever, silently
freezing state exactly like a half-open socket does." Push means importing that
class of problem onto a device with 264 KB of SRAM.

**Offline detection inverts and gets worse.** Today three failed polls at 1 Hz
means the device knows within ~3 s. Under push, a device cannot distinguish "no
updates because nothing changed" from "the server is gone", so a heartbeat is
required — which is polling. Detection then degrades to the heartbeat interval.

**Resync on reconnect.** The device still needs a full-state fetch path, so the
GET endpoints stay. Push is additive, not a replacement.

### Verdict

**No.** Push buys one thing the system is not short of and charges the exact
failure class this codebase has already paid for once, on the other hop.

Push becomes right if the server ever needs to reach a device that is not
currently asking — a second control surface (a phone, another panel) driving the
display, or sub-100 ms command latency. Neither exists today. Revisit then.

## Adaptive poll interval: evaluated and rejected

Recorded because it looks like the obvious answer under a noise framing, and
because understanding *why* it fails is what justifies the extra work in the
next section. The idea: poll slowly while idle, quickly while cooking.

`next_poll_interval_secs()` already exists and already tiers — it just tiers on
`fail_count` today. Adding an arm for `!snapshot.is_cooking()` is a small,
local change to a function with host-tested scheduling behind it
(`pico-core::scheduler`).

| | Idle traffic | Idle→cook latency | Work |
| --- | --- | --- | --- |
| Today | 86,400/day | ≤ 1 s | — |
| **Adaptive, 30 s idle** | **~2,880/day** | ≤ 30 s | small |
| Long-poll, 25 s hold | ~3,500/day | ~0 | moderate |

The striking part: **adaptive polling gets the same traffic reduction as
long-poll.** Both land around 3,000 requests/day, because both are bounded by
roughly the same interval. Long-poll's entire remaining advantage is the
transition latency — zero instead of up to 30 s.

So the real question is not "how much traffic can we remove" but **"does it
matter if the panel takes up to 30 s to notice a cook started from the phone?"**
Local input is unaffected — the encoder and button are handled on-device and can
force an immediate poll — so this only affects state changes originating
elsewhere.

### Decided: not acceptable

A cook started from the Anova app, or from a second panel, must appear
promptly. That requirement rules adaptive polling out as the primary answer.

The reason is structural, and it is the central fact of this document:

> **Plain polling couples latency to traffic.** The interval *is* the worst-case
> latency, so every request you remove is latency you add. There is no interval
> that is both quiet and responsive.

Halving the gap does not help much either — a 5 s idle interval still lags 5 s
and only cuts traffic ~5×, versus ~25× at 30 s. The trade is linear and there is
no sweet spot on it.

**Long-poll is the only option that decouples the two**, which is why it is the
recommendation rather than this. Adaptive intervals remain the right answer for
`/recipes`, which is hourly and changes rarely.

### The tradeoff nobody escapes

**Any reduction in idle chatter slows offline detection**, because you cannot
notice a dead server faster than you talk to it. At 1 Hz, `fail_count >= 3`
means ~3 s. At a 30 s idle interval it is ~35 s at best.

This is inherent, not an implementation detail — long-poll has the same problem
(a server that dies mid-hold is invisible until the hold or the client timeout
expires). Worth weighing now, because fast offline detection is load-bearing
while the TFT WiFi dropouts are still being diagnosed.

The principle that survives into the long-poll design: **idle *and succeeding*
is the only state that should be quiet.** Any failure should immediately return
the device to a fast, chatty mode. See "Where long-poll regresses" below for the
concrete form that takes.

## Long-poll: zero idle traffic *and* instant response

The device issues `GET /status?wait=25`; the server holds the request until the
state changes or the timeout expires, then answers normally.

- Latency → effectively zero, same as push — **this is its advantage over
  adaptive polling**, not the traffic reduction, which is comparable
- `/status` drops from 86,400 requests/day to ~3,500 — a ~25× cut in churn
- **No registration, no deregistration, no TTLs** — the device still initiates
- No inbound connectivity, no addressing, no NAT concerns
- Offline detection keeps its current character: a failed long-poll is a failed
  poll, and `fail_count` still works unchanged
- Nothing new to reconnect to

### ⚠ Where long-poll regresses: offline detection

The one thing that gets worse, and it matters because the whole point of this
change is responsiveness.

Today a dead server is noticed in ~3 s: polls fail fast and `fail_count >= 3` at
1 Hz. Under a 25 s hold:

- **Server exits cleanly** → TCP RST → the read fails immediately. Still fast.
- **Server hangs, or the network drops** → half-open socket → nothing until
  `API_CALL_TIMEOUT_SECS` expires, which has to be *above* the hold. Three of
  those is ~90 s.

The half-open case is the same failure `ws.rs` fights on the upstream hop, and
it is what makes naive long-poll feel broken when the network is flaky.

**Mitigation: treat a long-poll failure as a stronger signal than a poll
failure.** A failed long-poll means the connection itself broke, which is more
informative than a single missed quick poll. On failure, immediately retry once
with `?wait=0` (an ordinary fast poll); if that also fails, declare offline.
Detection becomes ~hold + 5 s in the worst case and near-instant in the common
one, without weakening `OFFLINE_THRESHOLD` for the normal path.

This needs designing in from the start — retrofitting it after the first flaky
afternoon is how `liveness.rs` came to exist on the other hop.

### It is one request and one response

Worth stating plainly, because this is the usual confusion. Long-poll does not
send multiple responses down one connection. The server simply answers late.

| | Requests : responses | Server holds? |
| --- | --- | --- |
| Normal poll | 1 : 1 | no |
| **Long-poll** | 1 : 1 | yes — until change or timeout |
| SSE / chunked | 1 : 1, body never ends | streams many events into one body |
| WebSocket | framed full-duplex after upgrade | persistent |

```
t=0.0   Pico:   GET /status?wait=25 ───────────▶
t=0.0   Server: handler awaits; sends NOTHING
        ... 8 seconds of silence on the socket ...
t=8.2   Server: state changes, handler wakes
t=8.2   Server: ◀─── 200 OK, Content-Length: 342, {json}
t=8.2   Pico:   read_to_end() returns, parse, re-ask
```

### Framing does not change

The device already reads `Content-Length`-framed bodies via
`response.body().read_to_end()` (`api.rs:94`), and reqwless already handles
that. **A delayed response goes through the identical path** — no new parsing,
no streaming state machine, no protocol work on the device.

This is the whole reason long-poll is cheap here, and the reason to prefer it
over SSE. SSE genuinely streams multiple events down one body, which is a better
conceptual fit, but `read_to_end()` is fundamentally incompatible with a body
that never ends: it would need incremental reads, `\n\n` event-boundary parsing,
partial-event buffering and bespoke idle-timeout logic. That is a real protocol
implementation on the MCU.

## ⚠ The subtle bug: the server does not know what the client has

A naive handler waits on a `watch` receiver:

```rust
let mut rx = s.status_rx.clone();
tokio::time::timeout(hold, rx.changed()).await;   // WRONG
Json(rx.borrow().clone())
```

"Changed" here is measured from **when this handler started waiting**, not from
what the client last received. Each request is a fresh task with no memory of
what the previous one sent, so there is a gap:

```
t=0.0  Server ──▶ 200 OK {state v5}
t=0.1  Client parses, updates display
t=0.15 ★ state changes to v6 ★         ← no handler is waiting
t=0.2  Client ──▶ GET /status?wait=25
t=0.2  Handler waits for the *next* change — v6 is never delivered
```

The server holding no per-client state is exactly what makes long-poll cheap, so
the client must supply the missing half.

**Damage is bounded, not unbounded**, provided the timeout arm returns the
*current* value rather than an error: worst case the client is one hold-period
stale (25 s), not stale forever. It degrades rather than breaks. Fix it anyway —
it is nearly free.

### The fix: the client echoes a version

The server assigns a monotonic version to each published state; the client sends
back the version it holds. The semantics become **"give me state newer than N"**:

```
Client ──▶ GET /status?since=5

Server: current_version > 5 → return immediately     (client missed v6)
        else                → wait for > 5, or timeout
        either way          → 200 { version: 7, ...state }

Client stores 7, loops.
```

The race disappears because the comparison is against what the client actually
holds, not against a server-side subscription instant.

**Carry it as a query parameter.** `api::request()` already builds URLs with
`alloc::format!("{server}{path}")`, so `?since=5` costs nothing and the version
rides in the JSON body. `ETag` / `If-None-Match` expresses the same idea in
standard HTTP clothing and would let the server answer `304` on timeout;
reqwless supports request headers so it is viable, but it is more plumbing for a
body that is already small.

### It also subsumes the reconnect resync

After a dropped connection, a WiFi blip, or a reboot carrying a persisted
version, the device asks with whatever version it has and immediately gets
current state if things moved. There is no separate "fetch full state on
reconnect" branch — it falls out of the same request. A real push architecture
would have had to build that explicitly.

## Endpoint grouping: resolved

**(a) Version each endpoint independently.** Three long-poll loops, three held
connections. Minimal change to `api_client.rs` — the existing `EventQueue` keeps
its `PollStatus` / `PollCurrentCook` / `PollRecipes` events, they just take
longer to return.

**(b) Collapse to one `GET /state?since=N`** returning whatever changed. One
held connection instead of three.

**Resolved: combine `/status` and `/current-cook`, leave `/recipes` alone.**

Both of the first two are small, frequent, 1024-byte-socket endpoints, and both
carry state a user expects to see immediately. Holding one connection for the
pair rather than two is the point of the exercise. `/recipes` is a
4096-byte-socket endpoint on an hourly cadence whose contents almost never
change; folding it into a 25-second-hold connection buys nothing and drags the
large-payload path into the redesign. It stays a plain hourly poll.

That takes 95,040 requests/day to ~3,500 without restructuring the large-payload
path. The cost is in `api_client.rs`'s scheduler, which manages the three
intervals independently today with `EventKind` variants and backoff tiers built
around per-endpoint polls — `PollStatus` and `PollCurrentCook` would merge.

## What the codebase already gives you

Worth knowing before estimating the work — a good deal of the server side exists.

- **`watch` channels are already in place**: `main.rs:196-199` creates
  `read_model_tx`, `status_tx`, `current_cook_tx` and `cook_progress_tx`. A
  handler can `changed().await` on one directly.
- **`read_model.rs` anticipates exactly this.** Its doc comment says the HTTP
  processor "will eventually consume this snapshot (via `watch`)". This is that.
- **The device's scheduler is host-tested.** `pico-core::scheduler` covers
  `EventQueue` ordering and the `EventKind` priority tie-break, so merging two
  poll events is a change with tests behind it.
- **Backoff and offline logic already exist** and mostly survive unchanged:
  `API_CALL_TIMEOUT_SECS`, `next_poll_interval_secs`'s tiers, `fail_count`,
  `OFFLINE_THRESHOLD`.

The genuinely new pieces are the version counter, the `since`/`wait` handler,
and cancellability on the device.

## Choosing the hold duration

**~50 seconds.** The reasoning, since the obvious constraint turns out not to be
the binding one:

| Constraint | Bound | Binding? |
| --- | --- | --- |
| Local command latency | see below | **yes, until the poll is cancellable** |
| Dead-server detection | hold + one fast retry | accepted at ~55 s |
| Middlebox default timeouts | just under 60 s | shapes the exact number |
| NAT idle timeout | n/a — LAN, no NAT in path | no |
| Diminishing returns | ~1,700 req/day from 25 s → 50 s | no |

**Avoid 60 exactly.** Common proxy defaults sit there — nginx's
`proxy_read_timeout` is 60 s. There is no proxy today, but if one is ever
introduced a 60 s hold gets cut at precisely the wrong moment and presents as
intermittent network flakiness. 50 s costs nothing and stays clear.

**The traffic gain past 25 s is marginal.** 25 s → ~3,456 requests/day; 50 s →
~1,728. That is ~1,700 saved on top of the ~83,000 the first move already saved.
Pick the number for the failure-detection budget, not the traffic curve.

**Hold length only matters while idle.** During a cook, state changes often
enough that the hold rarely runs to completion and request rate is driven by
Anova's push cadence instead. The figures above are idle-time figures — which is
exactly the period this document is about.

---

## Plan

Steps 1–2 are server-side and independent of the device. **Step 3 fixes a bug
that exists today and should be done first regardless of whether the rest of
this plan happens.** Step 0 is unrelated to everything else and may be the whole
fix for the logging half.

### Task 0 — settle the logging question (independent, minutes)

Device polls and `/status` handlers already log nothing at default levels. The
log noise is `anova_oven_server::processors::ws=debug` in the `EnvFilter`
default (`main.rs:66-69`), driven by Anova's push cadence and entirely
independent of the devices — see "Where the noise actually comes from".

Either drop `ws` to `info`, or make `ws.rs:406`/`:426` log on *change* rather
than per frame. **Nothing else in this plan affects log volume.**

### Task 1 — version the published state (server)

Attach a monotonic `u64` to the state the state-machine processor publishes,
incremented on each change. The `watch` channels already exist
(`main.rs:196-199`).

Without this, step 2 silently drops updates landing between a response and the
next request — see "The subtle bug" above. Do not skip it as an optimisation to
add later; it is the correctness core of the design.

### Task 2 — `?since=` / `?wait=` on a combined endpoint (server)

New `GET /state?since=N&wait=50` returning whatever `/status` and
`/current-cook` carry today, plus the version.

- `current_version > since` → return immediately
- otherwise `tokio::time::timeout(wait, rx.changed())`
- **either way return the current value with its version.** Never error on
  timeout — that would break `fail_count` on the device.
- Bound the hold server-side too, so a wedged handler cannot pin a connection.

`/recipes` is untouched: large payload, hourly, rarely changes.

### Task 3 — ⚠ make the device poll cancellable (device)

**Do this first, independently. It fixes a bug that exists today.**

`api_client.rs:429-447` selects only on the *wait* for the next event; once
`handle_event().await` starts it runs to completion. Commands are drained
between events, never during one.

This is not a cost that long-poll introduces — it is a **latent bug that
long-poll would amplify from 5 s to 50 s**. The next poll is enqueued *after*
the await, from `Instant::now()` (`api_client.rs:306-310`), so the cycle is
`poll_duration + interval` and the blocking duty cycle is:

| State | Poll takes | Interval | Blocked | Max delay |
| --- | --- | --- | --- | --- |
| Healthy | ~30 ms | 1 s | ~3% | ~30 ms |
| **Fails 1–4** (before backoff) | 5 s | 1 s | **~83%** | **5 s** |
| Tier 1 (5–9 fails) | 5 s | 5 s | ~50% | 5 s |
| Tier 3 (15+ fails) | 5 s | 30 s | ~14% | 5 s |

Healthy operation is masked twice: a ~3% chance of landing in the window, and
~30 ms of delay if you do — below perception. **The failure case is not
masked.** The worst duty cycle is the first few failures, before backoff
engages, because `next_poll_interval_secs()` returns `NORMAL_POLL_INTERVAL_SECS`
until five consecutive fails.

`OFFLINE_THRESHOLD` is 3. So at the exact moment the panel reads "Server
offline" and the user reaches for the encoder, the device is ~83% likely to be
mid-timeout and will swallow that input for up to five seconds. This is
plausibly already being observed and attributed to network slowness.

Note `poll_action_in_flight()` (`api_client.rs:196`) already addresses the
*scheduling* half — queued polls step aside for a pending `ApiStart`/`ApiStop`.
It cannot help the in-flight case, because reaching that check requires the
current `handle_event().await` to return.

With a 50 s hold, the same bug means **a user pressing Start sees nothing happen
for up to 50 s** — worse than the phone-initiated latency this whole plan exists
to avoid, and on their own button press.

`TcpClientState::<1, ...>` permits one socket, so a concurrent POST is not an
option either. The held poll must be droppable:

```rust
select(long_poll(...), commands.receive())
```

Dropping the future closes the socket. This is cancellation-**safe**, unlike the
SPI case in `docs/pico-display-refresh.md`: it is a GET, the server has mutated
nothing, and an abandoned connection is indistinguishable from a client
disconnect. The `rx_buf` borrow is released with the future.

Verify by pressing Start during a held poll and confirming the command goes out
immediately. Today's version of the test: pull the server's plug, wait for
`ServerOffline`, then press Stop and confirm it is not swallowed.

### Task 4 — long-poll the combined endpoint (device)

- Merge `EventKind::PollStatus` and `PollCurrentCook` into one event.
- Store the last-seen version beside the snapshot; send it as `?since=`.
- Raise `API_CALL_TIMEOUT_SECS` above the hold — 50 s hold, 55 s timeout.
- Keep the `EventQueue`, backoff tiers, `fail_count` and `OFFLINE_THRESHOLD`.

### Task 5 — preserve fast offline detection (device)

Without this, detection regresses from ~3 s to ~165 s (three 55 s timeouts).

On a *failed* long-poll, immediately retry once with `?wait=0` — an ordinary
fast poll. If that also fails, treat it as offline. A failed long-poll means the
connection broke, which is a stronger signal than one missed quick poll, so it
earns the shortcut. Detection becomes ~hold + 5 s worst case, near-instant when
the server exits cleanly and the socket RSTs.

The principle from the rejected adaptive-polling option carries over: **idle and
succeeding is the only state that should be quiet.**

### Task 6 — `api_heartbeat` will start lying

`bump_api_heartbeat()` sits at the top of the loop (`api_client.rs:427`), so a
50 s hold makes it advance every 50 s instead of every second. It stops
distinguishing "idle" from "wedged" — exactly when `/health` would be consulted.

Bump it on wake rather than per loop iteration, or record a last-activity
timestamp. Identical in shape to the `display_heartbeat` problem in
`docs/pico-display-refresh.md`, and worth fixing the same way.

### Verification

Server: unit-test the `since`/`wait` handler for return-immediately,
return-on-change, and return-on-timeout-with-current-value.

Device: `scripts/build-and-test.sh` across all six configs. The version-
comparison and merged scheduling logic belong in `pico-core` where they can be
host-tested; `pico-core::scheduler` already covers `EventQueue` behaviour.

On hardware, the three things only a bench can confirm: Start during a held poll
goes out immediately (task 3), a cook started from the phone appears within a
second (the objective), and pulling the server's plug surfaces `ServerOffline`
within ~a minute rather than hanging (task 5).

## Not doing

**Push with registration.** See the verdict above. Revisit only if the server
needs to reach a device that is not currently asking.

**SSE.** Genuinely streams events down one body, so there is no per-update
request at all. Materially more device work — `read_to_end()` cannot consume an
open-ended body — and it buys little over long-poll at these rates. The
direction if the latency requirement ever tightens further, not a plan.

**Adaptive poll intervals.** Evaluated and rejected above: same traffic
reduction, but couples latency to interval. Still the right answer for
`/recipes`, which keeps its hourly cadence.

### A note on the multi-client case

"A cook started from another panel" needs no extra design. Panel A issues
`POST /start`, the server state moves, and every device holding a long-poll is
answered immediately. The fan-out falls out of the `watch` channel — one more
reason this is cheaper than push, where each device would need tracking and
individual delivery.

### The external trigger that would raise priority

If the TFT WiFi dropouts turn out to be power or RF related, cutting connection
churn ~50× meaningfully reduces radio TX duty cycle. **Diagnose first.** If it
is the 3V3 rail (leading theory: ~60–100 mA of TFT sharing the CYW43439's
supply), moving the TFT to VSYS fixes it properly and the transport is beside
the point.
