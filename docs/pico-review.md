# `anova-oven-pico` crate evaluation

Reviewer: Claude (static review + `cargo check`/`clippy` on `thumbv6m-none-eabi`).
Scope: all 10 source files (~2.87 kLOC), `Cargo.toml`, `build.rs`, `memory.x`, `.cargo/config.toml`.
Note: file/line references below are pinned to the snapshot at review time; some modules have since been renamed (notably `app_state.rs` → `state.rs`) and `health.rs` was added after the original review. Findings are still valid against current code unless a `✅ DONE` annotation says otherwise.
Build status: compiles clean in `--release`; clippy reports only two dead-code variants
(`EnqueueMode::Replace`, `BacklightPolicy::Dim`).

Severity legend: **[H]** correctness/reliability hazard · **[M]** meaningful improvement ·
**[L]** polish/idiom.

---

## 1. Concurrency & hardware–software interaction

### 1.1 [H] ✅ DONE `static mut HTTP_RX_BUF` is UB-adjacent and unscalable — `api.rs:15,29-30`

> Fixed in `d0f7b15`: buffer owned via `StaticCell`, threaded by `&mut`; all `unsafe`/`static_mut_refs` removed.

All five network functions take `&mut HTTP_RX_BUF.0` (a 16 KB `static mut`) under
`#[allow(static_mut_refs)]`. It is *currently* sound only because of an unwritten,
unenforced invariant: exactly one task (`api_client_task`) ever calls these, and it
`.await`s each call to completion before the next, so two `&mut` aliases never coexist.
Nothing in the type system protects this. The day a second caller appears, or an API
call is ever `select!`-ed against something re-entrant, this is instant aliasing UB on a
target with no MMU to catch it.

Recommendation: delete the `static mut` and the `unsafe`. Make the RX buffer an owned
field of `ApiRuntime` (heap `Box`/`Vec`, or a `static_cell::StaticCell<Aligned<16384>>`
taken once at construction) and thread `&mut` through `handle_event`. This is the single
highest-value change in the crate: it removes the only `unsafe` aliasing in the codebase
and makes the "one buffer, one user" rule a compile-time fact.

### 1.2 [H] ✅ DONE Stop/Start latency behind the poll-drain loop — `api_client.rs:495-510`

> Fixed in `164ea20`: command channel serviced before each event in the drain loop; initial polls staggered 0/250/500 ms. Drain loop rewritten as an explicit `match`/`break`; Start-coalescing documented.

The task loop does `select(Timer::at(next_due), commands.receive())`. When the timer
wins, it runs `while let Some(event) = pop_due(now) { handle_event(event).await }`,
draining **every** due event before returning to the `select`. During that drain the
command channel is not serviced. Each `handle_event` can block up to
`API_CALL_TIMEOUT_SECS` (5 s). At boot all three polls (`PollStatus`,
`PollCurrentCook`, `PollRecipes`) are enqueued at `now` (`api_client.rs:235-237`), so the
first drain can take ~15 s of back-to-back network I/O. A `Stop` issued by the user in
that window sits unread in the channel (capacity 4) for many seconds, then risks being
dropped on overflow. For an appliance whose `Stop` cuts power to a hot oven, multi-second
unbounded `Stop` latency is the most user-visible reliability problem here.

The `EventQueue` already models priority (`ApiStart`/`ApiStop` = 0) but `soonest_index`
sorts by `execution_time` first and only breaks *ties* by priority — and more importantly
the command isn't even moved from the channel into the queue until the drain finishes.

Recommendations (any of, in order of value):
- Service the command channel between events inside the drain loop (e.g.
  `commands.try_receive()` each iteration, handle immediately).
- Stagger the three initial polls (e.g. `now`, `now+250ms`, `now+500ms`) so the first
  drain isn't a 15 s block.
- Consider lowering `API_CALL_TIMEOUT_SECS`, or giving `send_stop` a shorter timeout than
  reads.

### 1.3 [M] `persist` "atomic u32, no critical section" comment is misleading — `persist.rs:37-39,395-400`
`bump()` is a `read_volatile` → `+1` → `write_volatile` read-modify-write, which is *not*
atomic. The code is in fact race-free, but for a different reason than the comment states:
the executor is single-core, cooperatively scheduled (`executor-thread`), `bump()`
contains no `.await`, and each breadcrumb field has exactly one writer task
(api/display/watchdog). The panic/HardFault path additionally `cortex_m::interrupt::disable()`s
first. Reword the module comment to state the real invariant (single-core + no await +
single-writer-per-field); the current wording invites someone to add a second writer
believing the RMW is safe. Functionally **no bug today**.

### 1.4 [M] LCD-init panic → unrecoverable fast reset loop — `main.rs:145`
`panic!("LCD init failed")` flows into the `#[panic_handler]`, which records and
`SCB::sys_reset()`s immediately. If the LCD is genuinely dead, every boot panics in the
first ~100 ms and resets forever — the device is bricked with no network, no `Stop`
capability, hammering reset. `spawner.spawn(...).unwrap()` calls at init have the same
shape. Consider a degraded headless mode (skip the LCD, still bring up WiFi + API so the
oven remains controllable) or at minimum a bounded retry before giving up.

### 1.5 [L] Rotary encoder can drop counts on fast rotation — `input.rs:68-72`
After an edge, `Timer::after(500µs)` then a single sample. Edges that occur between
`select` completing and the next `wait_for_any_edge` arming are not latched by
embassy-rp's async GPIO. Fine for hand speed; fast spins lose detents. The QEM table +
accumulator decode itself is correct and the direction-reversal reset is a nice touch.
Acceptable for this use case — note it, don't necessarily fix it.

### 1.6 [L] Button task: 500 ms blanking, no release wait — `input.rs:44-53`
`wait_for_falling_edge` then unconditional `Timer::after(500ms)`. A held button emits one
event per 500 ms; double-clicks within 500 ms are swallowed. Likely intentional debounce
but the value is large and it debounces by time rather than by release edge. Minor UX nit.

### 1.7 [L] Hardcoded network seed — `main.rs:251`
`seed = 0x0123_4567_89ab_cdef` feeds embassy-net's TCP ISN / ephemeral port
randomization, identical on every device and every boot. Low risk on a trusted LAN, but
trivially improved by seeding from ROSC/RNG.

### 1.8 [H] ✅ DONE `/current-cook` parse failure is misclassified as a successful poll — `api_client.rs:420-425`, `api.rs:151-155`

> Fixed in `f845b82`: `fetch_current_cook` returns `Result<Option<CurrentCook>, ()>` (orthogonal success/presence axes); only `Ok(_)` counts as success. Typed error deferred to §6.1.

`fetch_current_cook` returns `None` *both* for a legitimate HTTP 204 (no cook in
progress) *and* for a JSON parse failure. `handle_poll_current_cook` matches the
`with_timeout` result as `Ok(current_cook)` and unconditionally calls
`record_fast_poll_success()`, resetting `fail_count`. Consequence: if `/current-cook`
consistently returns a malformed body, every poll is counted as a success and the device
**never transitions to `Offline`**, so the UI keeps showing stale data instead of the
server-offline view. This is a direct consequence of the `Option`-as-error idiom (see
§6.1) erasing the difference between "no cook" and "parse failed". Fix by returning a
`Result`/three-state value that distinguishes 204 from a decode error, and only counting
204 (and real successes) as a successful poll.

---

## 2. Performance & resource bottlenecks

### 2.1 [H] 🟡 PARTIAL Allocation churn on a 32 KB first-fit heap, forever — `lcd.rs`, `api.rs:17-24`, `display.rs`

> `c033420`: `normalize_server_url` now computed once at startup, not per request. Remaining: per-tick LCD `alloc::format!` → `heapless`; `Arc`-wrap status/cook; keep heap monitor in release.

This is the dominant long-term reliability risk for a device meant to run unattended for
days next to a hot oven.

- `display_task` re-renders every `ANIM_TICK_MS` (50 ms) for the lifetime of the device.
  `render_status_display` issues several `alloc::format!`/`String` allocations *before*
  the `write_row` dedup check can short-circuit, so the heap sees a steady stream of
  small alloc/free every 50 ms indefinitely.
- `normalize_server_url` does an `alloc::format!`/`String` on **every** API call (≈ every
  second at the normal poll rate) even though `SERVER_URL` is a compile-time `env!`
  constant that never changes.
- Each `ViewSpec::Status` clones full `OvenStatus` + `CurrentCook` (owned `String`s/`Vec`s)
  per render.
- Recipe JSON is `serde_json`-parsed from the 16 KB buffer into `Vec<Recipe>` (many small
  interleaved allocations).

`embedded_alloc::LlffHeap` is linked-list first-fit: mixing many small short-lived strings
with occasional large allocations (16 KB-fed recipe parse) fragments the 32 KB arena over
time. There is a `heap_monitor_task` but it only logs (behind `verbose-logs`); it takes no
corrective action and won't be running in production builds.

Recommendations:
- LCD rows are ≤16 chars — format them with `heapless::String<32>` (the pattern already
  exists in `render_recovery`) and eliminate the per-tick `alloc::format!` entirely.
- Compute the normalized server URL once (const-fold, `OnceCell`, or store on
  `ApiRuntime`).
- Consider `Arc`-wrapping `status`/`cook` in the snapshot the way `recipes` already is,
  so renders clone a pointer not the payload.
- Keep the heap monitor (or a `min_free` watermark) compiled into release as a cheap
  guard.

### 2.2 [M] No connection/DNS reuse — `api.rs`
Every poll constructs a fresh `TcpClientState` + `TcpClient` + `DnsSocket` + `HttpClient`,
opens a new TCP connection, and (if `SERVER_URL` is a hostname) issues a DNS query — at
~1 Hz, forever. That's one full connect/teardown (and possibly one DNS round trip) per
second. Caching the resolved `IpAddress` and/or holding a keep-alive connection would cut
latency, radio time, and server load substantially.

### 2.3 [M] Large `TcpClientState` lives in the task future — `api.rs:32,102,162,196,238`
`fetch_recipes` allocates `TcpClientState::<1,4096,4096>` (~8 KB) on the stack/future;
others use 1024+1024. Because these are locals held across `.await`, they inflate the
`api_client_task` future (statically sized by embassy). Not a stack overflow, but worth
being aware of given the 264 KB SRAM is also feeding a 32 KB heap, the 16 KB RX buffer,
cyw43 firmware/NVRAM blobs, and `StackResources<16>`. A quick `cargo size`/map check of
the final binary's `.bss`/`.data` headroom would be prudent.

### 2.4 [L] `fetch_recipes` silently empties the menu if body > 16 KB — `api.rs:266-296`
`read_to_end` into the shared 16 KB buffer fails for larger payloads; the UI then loses
the recipe list entirely with only a `warn!`. Acceptable if the server guarantees small
payloads — document that contract, or paginate.

---

## 3. Idiom / structure / maintainability

### 3.1 [M] `is_cooking()` stringly-typed on `"idle"` — `api_client.rs:77-83`, `state.rs:15,180-184`
Cooking detection hinges on `status.mode.as_str() != "idle"` and `optimistic_idle_view`
writes the literal `"idle"` back. This contract with `anova-oven-api` is implicit and
fragile. An enum (or a typed `is_idle()` method) in `anova-oven-api` would make the state
machine robust to server wording changes.

### 3.2 [M] ✅ DONE API surface duplicated five times — `api.rs:26-298`

> Fixed in `d5d1a3a` (same commit as §6.1): extracted a single generic `request<R, F, const TX, const RX>(…, handler)` helper; each endpoint now contains only its per-endpoint status branching + serde. `celcius_to_fahrenheit` typo left for a future cosmetic pass.

`fetch_status`, `fetch_current_cook`, `fetch_recipes`, `send_stop`, `send_start` repeat
the same 20-line "build client / request / send / check status / read body" boilerplate
with slightly different error logging. Extract a single helper
(`request(method, path, body) -> Result<&[u8], _>`) and keep only the per-endpoint
serde/branch logic. Also unifies the duplicated `celcius_to_fahrenheit` (defined in both
`api.rs:300` and `lcd.rs:439`; typo "celcius" in both — rename to `celsius_to_fahrenheit`).

### 3.3 [L] Dead variants flagged by clippy — `api_client.rs:114`, `state.rs:49`
`EnqueueMode::Replace` and `BacklightPolicy::Dim` are never constructed. Either wire them
up or remove them; `BacklightPolicy` collapsing to just `Full`/`FullThenDimAfter` would
also simplify `state.rs:108-127`.

### 3.4 [L] Magic numbers / addresses
`WATCHDOG_REASON_ADDR = 0x4005_8008` (`persist.rs:64`) and the LCD degree glyph `0xDF`
(`lcd.rs:264`) are well-commented but raw. `enable_tick_generation(12)` (`main.rs:199`)
hardcodes the 12 MHz `clk_ref` assumption — a `const` with the rationale would help.
`PersistRegion` offsets are hand-documented in the module comment and must be kept in sync
manually; a `const_assert!` on `size_of`/field offsets would catch silent drift when the
struct changes (the `MAGIC` bump only catches it at runtime on the *next* device).

### 3.5 [L] `.uninit.PERSIST` placement relies on cortex-m-rt defaults — `persist.rs:91`, `memory.x`
The persist region's survival across resets depends on `.uninit` being NOLOAD and not
zeroed — correct with cortex-m-rt — but `memory.x` doesn't pin its address. A firmware
layout change can relocate it; this is handled gracefully by the `MAGIC` check (old data
is discarded), so it's sound, just worth a comment in `memory.x` so the dependency is
discoverable.

---

## Priority shortlist

1. ~~**1.1** — remove `static mut HTTP_RX_BUF`; own the buffer (kills the only aliasing `unsafe`).~~ ✅ DONE (`d0f7b15`)
2. ~~**1.2** — make `Stop`/`Start` preempt the poll-drain loop; stagger initial polls.~~ ✅ DONE (`164ea20`)
3. **2.1** — 🟡 compute server URL once ✅ DONE (`c033420`); bound LCD formatting with `heapless` still pending.
4. **1.4** — degraded mode instead of panic-reset-loop on LCD failure.
5. **2.2 / 3.2** — connection/DNS reuse + ~~de-duplicate the five API functions~~ ✅ §3.2 DONE (`d5d1a3a`); §2.2 keep-alive/DNS-cache still pending.
6. **1.3** — correct the `persist` safety comment (no code change).

No race conditions cause incorrect behaviour *today* (single-core cooperative scheduling
saves several near-misses), but 1.1 and 1.3 are latent traps and 1.2/2.1 are real
operational problems under load and over long uptimes.

---

## 4. Embassy idiom & architecture assessment

Overall: this is **more idiomatic than most hobby embassy firmware**. The instincts are
good and consistent; the non-idiomatic parts are concentrated in the `alloc` lean and the
hand-rolled API scheduler. The encoder/button/LCD/state-machine half is well-matched to
the user-facing constraints and should be kept essentially as-is.

### 4.1 What's idiomatic and good (keep it)
- **Consistent actor/handle pattern** — `Display`/`Input`/`ApiClient` each spawn a task in
  `new()` and return a cheap handle over a `&'static` sync primitive.
- **Correct sync-primitive selection** — `Signal` for the display (latest-wins is exactly
  right), `Channel` for discrete input/command events, `Watch` for shared state with
  change-notify. Each picked correctly rather than defaulting to `Channel` everywhere.
- **`select`/`select3` + `Either` FSM** — `loop { state = state.execute(&mut ctx).await }`
  with per-state async handlers is a clean sans-IO-style state machine. The optimistic-UI
  handling is a thoughtful answer to a high-latency polled backend.
- **Async LCD delays** — `embassy_time::Delay` with the non-blocking HD44780 driver yields
  during ms-scale strobes instead of stalling the executor (a common mistake avoided).
- **`portable-atomic`/`critical-section`, single-core, watchdog-as-task** — correct
  choices. Not using core1 is the *right* call for this workload.
- **Display/animation split** — FSM decides *what*, `LcdController` owns *how/scroll*.
  Correct separation; do not change.
- **Polling-an-intermediary-server boundary** — simpler than embedded WebSocket; the
  server absorbing the cloud connection is a sound split. Keep.

### 4.2 Non-idiomatic vs embedded-Rust convention
- **[M] The heavy `alloc` lean is the least embedded-idiomatic thing in the crate.** Full
  `serde_json` + `String`/`Vec`/`format!` on a 32 KB `LlffHeap` is pragmatic but against
  the grain; convention is `heapless` + `serde-json-core` (no-alloc). Everything here is
  bounded in practice (16-char LCD, finite recipe list), so this is feasible, not just
  theoretical. (Ties to §2.1.)
- **[L] Mixed spawn styles** — some sites use `?`/`Result`, others
  `spawner.spawn(task().unwrap())`. Idiomatic helper for infallible init spawns is
  `spawner.must_spawn(task())`. Unify.
- **[L] `CriticalSectionRawMutex` on task-only channels** — nothing crosses an ISR
  boundary, so `NoopRawMutex` is sufficient and cheaper. CS is a defensible conservative
  default; just heavier than the situation needs.

### 4.3 Architectural changes I would make
Listed by impact. #1 and #2 are near-pure wins; #3 is higher-effort with a cross-crate
ripple and should be weighed most carefully.

**1. [H] Replace the hand-rolled `EventQueue` scheduler with one task per poll concern.**
Tasks are free in embassy. `status_poll_task` / `cook_poll_task` / `recipes_poll_task`
(each its own `Timer` + backoff loop) plus a `command_task`, all writing the shared
`Watch`, would dissolve the custom `EventQueue`, the `EnqueueMode`/priority logic, the
`poll_action_in_flight` re-queue dance, **and** the Stop/Start-latency problem (§1.2) —
a command task is never stuck behind a poll drain. Trade-off: each task needs its own RX
buffer/socket (RAM + concurrent TCP sockets). Mitigations: recipes polls hourly so it can
share/reuse a buffer; status/cook payloads are ~1 KB. Trades a clever ~250-line scheduler
for boring, independently-testable tasks and removes two report issues for free.

**2. [M] Close the watchdog↔heartbeat loop.** The persist region already records
`api_heartbeat`/`display_heartbeat`, but `watchdog_feeder_task` feeds *unconditionally* —
it only proves the executor is alive, not that the API/display tasks are progressing. A
hung-but-not-panicking task won't trigger recovery. Have the feeder snapshot the
heartbeats and only `feed()` if they advanced (with generous margins). The infrastructure
is already 90% built; this small change makes the watchdog actually meaningful.

**3. [M] Drop the allocator entirely.** Bound recipes (`heapless::Vec<Recipe, N>` with
`heapless::String` fields), parse with `serde-json-core`, format LCD rows with
`heapless::String<32>` (pattern already exists in `render_recovery`). This eliminates
§2.1 (multi-day heap fragmentation) as a *class* of problem rather than mitigating it.
Cost: a `no_std`/heapless-friendly (feature-gated) variant of the shared `anova-oven-api`
types — the only item here with a real cross-crate ripple.

Do **not** change: the FSM structure, the display/animation split, the
polling-an-intermediary-server boundary, or the single-core executor.

---

## 5. Production-readiness gaps

The device *logic* is solid and several subsystems are already production-minded: the
persist/ring-buffer crash recorder, the layout-versioned `MAGIC`, the pinned
`rust-toolchain.toml`, the watchdog, and the tiered backoff are all above hobby grade.
The gap to production is not per-line code quality — it is the surrounding system
disciplines that let you trust the device unattended in someone's home and diagnose it
when it misbehaves.

### 5.1 [H] No tests
Largest gap. There are zero tests. All hardware-independent logic is pure and currently
unverified: the FSM transition table (`state.rs`), the QEM quadrature decode + accumulator
(`input.rs`), the scroll-window math (`lcd.rs::visible_window`), reset classification and
the ring buffer (`persist.rs`), `normalize_server_url`, and JSON (de)serialization against
recorded fixtures. The standalone-workspace constraint does not block this — factor the
pure logic into a `no_std` lib crate with a `std` dev-dependency test target. No
HIL/smoke test on real hardware in CI either.

### 5.2 [H] No CI gate
`.github/` is new/untracked. Expect a workflow that builds the firmware, runs
`clippy -D warnings` (the crate currently ships with dead-code warnings), runs host tests,
and enforces a `cargo size` check that the image still fits SRAM with a stated margin.
Nothing currently stops a regression.

### 5.3 [M] No explicit resource budget
Heap (32 KB), RX buffer (16 KB), `StackResources<16>`, and the large `TcpClientState` in
the task future are all unstated and unasserted. Production has a written memory budget;
the `heap_monitor` (here log-only and behind `verbose-logs`) should be a hard watermark
check compiled into release.

### 5.4 [H] No field observability or update path
Logging is defmt-RTT — requires a physically attached probe. Once deployed the only
failure signal is the LCD recovery screen. Crash breadcrumbs are recorded but never
exfiltrated; production would POST the last reset reason / panic to the server on boot.
No OTA story: single image, no bootloader / A-B partition (`embassy-boot`), update means
physical reflash. For an appliance, a serious gap.

### 5.5 [H] Safety case is implicit
This device applies heat. Production firmware for that has a written hazard analysis:
if the controller panics / loses WiFi / the LCD dies, what state is the oven left in and
what is the safe fallback? The actual posture (the Pico is a remote; the oven keeps its
own state if the Pico resets) is probably sound but is nowhere argued — and §1.4
(LCD-dead → infinite reset loop) is exactly the partial-failure mode a hazard review
would catch.

### 5.6 [M] Security posture undecided
No TLS and no auth on the control path is stated as a convenience ("server is on local
network") rather than threat-modeled. "Anyone on the WiFi can POST `/start` to a device
that applies heat" is a decision that should be made deliberately and documented.
Compile-time `env!` secrets also bake the WiFi password into the binary in plaintext with
no re-provisioning path.

### 5.7 [M] No firmware provenance
No version string / git SHA compiled into the banner or the persist region. When a field
unit misbehaves you cannot tell what is running on it. The persist *layout* is versioned;
the firmware itself is not.

### 5.8 [M] No error-handling policy
Pervasive `.ok()` / swallow-and-`warn!` with no escalation tier. Acceptable for a single
LCD glyph; not as a blanket policy — repeated LCD or network failures are invisible
without a probe and never escalate. Production distinguishes recoverable-and-continue
from must-escalate and states which is which.

---

## 6. Rust idiom — code craft

Distinct from the embassy/architecture assessment in §4: this section evaluates the Rust
itself — error handling, functional vs imperative style, function size, visibility, and
type design. Overall the style is competent idiomatic Rust with good combinator instincts;
the one category that is genuinely not idiomatic (`Option`-as-error) is also the one
actively concealing a correctness bug (§1.8).

### 6.1 [H] ✅ DONE Error handling — `Option`-as-error — `api.rs`

> Fixed in `d5d1a3a`: introduced `ApiError` enum (Connect/Send/BodyRead/Http(u16)/Json); all five api functions now return `Result<_, ApiError>`. The §1.8 `Result<Option<CurrentCook>, ()>` placeholder got its typed error. Underlying reqwless/serde error detail is now preserved in logs via `Debug2Format` instead of being discarded by `Err(_)`.

`api.rs` returns `Option<T>` (and an empty `Vec` for recipes) instead of
`Result<T, ApiError>`. This collapses five distinct failures — connect, send, non-2xx,
body-read, JSON parse — into a single `None` differentiated only by a `warn!` string.
`thiserror` is already a transitive dependency; an error enum + `?` would also delete the
`match { Ok(r) => r, Err(_) => { warn!; return None } }` boilerplate repeated ~5×. The
direct fallout of this idiom is the §1.8 misclassification bug (204 vs parse-failure
indistinguishable). Discarding `serde_json::Error` with `Err(_)` also loses parse detail
that `defmt::Debug2Format` could surface. (`.ok()` on LCD ops and `.unwrap()` on
init-time spawns — the latter better as `must_spawn`, see §4.2 — are defensible.)

### 6.2 [M] Functional vs imperative — two imperative smells
Idiomatic combinator use is present and good: `EventQueue::soonest_index` (`min_by` +
`.then()`), `next_stage_prompt`, `active_recipe_title` (`?`-chains). The exceptions:
- `render_status_display` ([lcd.rs:190-241](../crates/anova-oven-pico/src/lcd.rs#L190-L241))
  uses a manual `slot_idx` running counter compared against the active slot — the
  manual-index pattern that data-driven code exists to avoid. Build a
  `heapless::Vec` of row-1 candidates and index it; removes the slot/`num_items` sync
  hazard.
- The 7 `execute_*` handlers in `state.rs` each repeat
  `loop { snapshot; if offline; if cooking; render; select }` with minor variation. A
  shared "common transitions" helper returning `Option<AppState>` would DRY it.
- `persist.rs`'s volatile MMIO code is appropriately imperative, but `zero_region`'s ~12
  copy-pasted `write_volatile(addr_of_mut!(...), 0)` lines want a local helper/macro.

### 6.3 [M] Function size/scope — one clear outlier
`main()` (~200 lines: heap → persist → LCD → recovery → watchdog → wifi → dhcp → loop)
should decompose into `init_*` helpers. `render_status_display` (~130 lines; three modes
+ slot rotation + raw byte writes) is the other split candidate. `init_at_boot` is long
but cohesive (acceptable). Everything else is well-sized.

### 6.4 [L] Visibility — needlessly public for a binary crate
This is a `[[bin]]` with no `lib.rs`; nothing is consumed externally, yet
`Display`/`Input`/`ApiClient` are `pub` while `LcdController`/`BacklightController` are
`pub(crate)`. Idiomatic default for a bin crate is private/`pub(crate)`; the `pub` items
are noise that misrepresents the API surface. Harmless functionally. The `persist`
free-function API over `static mut` is an implicit global singleton — defensible since it
is a fixed RAM region, but a ZST token passed to tasks would make the dependency explicit
and testable; note the tradeoff rather than necessarily change it.

### 6.5 [L] Type design — missed newtype opportunities
- `write_row(row: u8, …)` with `if row == 0 { … } else { … }` treats any non-zero value
  as row 1. A `Row` enum (`Row0`/`Row1`) removes the foot-gun.
- `AppState::discriminant() -> u32` is a hand-maintained match with a "never renumber"
  comment. A dedicated `#[repr(u32)] enum AppStateId` with `From<&AppState>` localizes
  the persisted-ID contract instead of relying on a comment.
- Good idiomatic instance to keep: the `Aligned<const N: usize>` newtype
  (const generics + `repr(align)`).
