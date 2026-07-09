# Architecture and Implementation Plan

## Project Goal

Build a local server, a Rust CLI, and Raspberry Pi Pico W firmware to control
an Anova Precision Oven v1 as a replacement for the Anova mobile app.

Target capabilities:
- Read oven state (temperature, heating elements, steam, timer, etc.)
- List user recipes and cook history
- Send cook commands (`CMD_APO_START`, `CMD_APO_STOP`)

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────────┐
│  anova-oven-server  (Linux/macOS, full std)              │
│                                                          │
│  axum HTTP API  (plain HTTP, local network)              │
│  ├── GET  /status         → oven state JSON              │
│  ├── GET  /recipes        → recipe list JSON             │
│  ├── POST /update-recipes → force-refresh + return list  │
│  ├── GET  /history        → cook history JSON            │
│  ├── GET  /current-cook   → in-progress cook JSON or 204 │
│  ├── POST /start          → start cook from recipe       │
│  └── POST /stop           → CMD_APO_STOP                 │
│                                                          │
│  Processor-based runtime (typed mpsc/watch channels):    │
│  ├── state_machine processor — central decision logic    │
│  ├── ws processor — persistent WebSocket to              │
│  │     devices.anovaculinary.io (TLS 1.2, tokio-         │
│  │     websockets + native-tls)                          │
│  ├── firestore processor — Firebase auth + Firestore     │
│  │     queries (reqwest)                                 │
│  ├── http processor — axum router, translates requests   │
│  │     into StateMachineCommand                          │
│  └── cook_progress task — derives CookProgress from      │
│        status + current_cook over time                   │
└───────┬────────────────────────────────────┬─────────────┘
        │ plain HTTP, local network (no TLS) │
  ┌─────┴───────────┐         ┌──────────────┴──────┐
  │  anova-oven-    │         │  anova-oven-cli     │
  │  pico           │         │                     │
  │                 │         │  Desktop binary     │
  │  Pico W target  │         │  using the same     │
  │  (RP2040,       │         │  anova-oven-api     │
  │  no_std + alloc)│         │  types over the     │
  │  Connects to    │         │  same HTTP server.  │
  │  server over    │         │                     │
  │  plain HTTP     │         │                     │
  │  (embassy-net,  │         │                     │
  │  no TLS).       │         │                     │
  │  LCD + encoder  │         │                     │
  │  + button +     │         │                     │
  │  /health        │         │                     │
  │  (picoserve).   │         │                     │
  └────────┬────────┘         └──────────┬──────────┘
           │                             │
           │     crate dependency        │
           └──────────┬──────────────────┘
                      │
           ┌──────────▼──────────┐
           │  anova-oven-api     │
           │  (no_std + alloc)   │
           │                     │
           │  Shared request /   │
           │  response types     │
           │  (OvenStatus,       │
           │  Recipe, Stage,     │
           │  CurrentCook,       │
           │  CookProgress,      │
           │  HistoryEntry)      │
           └─────────────────────┘

           ┌─────────────────────┐
           │  anova-oven-pico-   │
           │  core               │
           │  (no_std + alloc,   │
           │  host-testable)     │
           │                     │
           │  Pure-logic library │
           │  used only by the   │
           │  pico bin: FSM      │
           │  shapes, persist    │
           │  data types, reset  │
           │  classification,    │
           │  scheduler, encoder │
           │  decode.            │
           └─────────────────────┘
```

### Why this split

`devices.anovaculinary.io` speaks TLS 1.2 only (confirmed: `openssl s_client
-tls1_3` receives a 7-byte CloseNotify; `-tls1_2` completes with
`ECDHE-RSA-AES128-GCM-SHA256`). `embedded-tls`, the only viable no_std TLS
library for the RP2040, implements TLS 1.3 only. A self-contained Pico W that
speaks directly to the Anova WebSocket API is therefore not achievable with
the current library ecosystem.

The local server removes every embedded constraint: TLS version, Firebase auth,
Firestore JSON parsing, WebSocket reconnection logic, and heap size all move to
the server. The Pico W becomes a thin client over plain HTTP on the local
network.

The CLI mirrors the embedded client's data flow — same `anova-oven-api` types,
same HTTP calls to the same server — so UI/UX can be prototyped on the desktop
before being ported to the Pico.

The split between `anova-oven-pico` (firmware bin, chip-specific) and
`anova-oven-pico-core` (pure logic, host-testable) exists so the firmware's
state machine, scheduler, encoder decode, persist-region data shapes, and
reset classification can be unit-tested under plain `cargo test` even though
the bin only builds for `thumbv6m-none-eabi`. See
[`docs/pico-crate-drift.md`](pico-crate-drift.md) for the field-by-field
ownership map.

---

## Crate Structure

```
crates/
  anova-oven-api/        # no_std + alloc — shared HTTP types
  anova-oven-pico-core/  # no_std + alloc — pure-logic firmware support
  anova-oven-server/     # std — local HTTP server (axum + tokio)
  anova-oven-cli/        # std — desktop CLI binary
  anova-oven-pico/       # no_std — Pico W firmware (standalone workspace)
```

Workspace members: `anova-oven-api`, `anova-oven-server`, `anova-oven-cli`,
`anova-oven-pico-core`. The pico firmware crate is a **standalone workspace**
because `critical-section` features conflict between `thumbv6m-none-eabi`
embedded crates and host-target crates that share an arena.

### `anova-oven-api` (no_std + alloc, optional `std`)

Defines the JSON request/response types served by `anova-oven-server` and
consumed by both `anova-oven-cli` and `anova-oven-pico`. No knowledge of
WebSockets, Firebase, or Firestore.

**Types** (see [`crates/anova-oven-api/src/lib.rs`](../crates/anova-oven-api/src/lib.rs)
for the authoritative list of fields — the summary below names the public
types, not every field):

- **`OvenStatus`** — the full oven state served by `GET /status`. Covers
  mode/phase, all temperature bulbs (dry top/bottom, wet, probe, target),
  timer, steam (current/target/generator), boiler (temperature/watts/descale
  flag), evaporator, fan speed, every heating element + wattage, lamp,
  vent, door, water tank, the oven's reported `activeStageIndex`/
  `activeStageId`, and an optional `cook_progress: Option<CookProgress>`.
- **`CookProgress`** — server-derived cook progression (recipe title,
  current stage index, total stage count, current/next stage description
  and kind, `next_stage_ready` flag for manual-advance prompts). Included
  inline on `GET /status` while a cook is active rather than requiring the
  pico to re-decode `CurrentCook` every tick.
- **`Recipe` + `Stage`** — saved recipe with stages. `Stage` carries
  Firestore stage id (used as `stageId` for `CMD_APO_START_STAGE` — see
  exploration note below), kind, target temperature, optional bulb mode,
  duration, timer/probe flags, probe target, steam %, fan speed,
  `user_action_required`, rack position, heating-element flags, vent, and
  title. `Recipe::normalize()` and `Stage::normalize_fan_speed()` enforce
  Anova's "fan must be 100% with rear heat or steam" rule.
- **`HistoryEntry`** — recipe title (or `"[manual]"`), `ended_at` (ISO 8601),
  stage count.
- **`CurrentCook`** — the in-progress cook served by `GET /current-cook`:
  recipe title (or `"[manual]"`), optional `recipe_id`, `started_at`,
  stages, plus `cook_stage_count` (excluding preheat) and
  `total_stage_count`.

All types derive `serde::Serialize` + `serde::Deserialize` with
`default-features = false` so they compile for `thumbv6m-none-eabi`.

**Key design decisions:**
- Temperatures are always Celsius; the CLI/Pico convert for display.
- Stages are pre-filtered to `stepType == "stage"` (directions stripped).
- `stage_count` is included at the top of `Recipe` for list views that
  don't need to decode the full `stages` array.
- History entries resolve recipe titles server-side; `"[manual]"` is used
  when a recipe document can't be fetched.
- `CookProgress` lives on `OvenStatus`, not on `CurrentCook`, so the pico's
  1 Hz `/status` polling carries enough data to drive the LCD without an
  extra `/current-cook` round trip every tick.

### `anova-oven-pico-core` (no_std + alloc)

Pure-logic library extracted from the firmware. Host-testable via plain
`cargo test`. No MMIO, no chip HAL, no radio driver, no logging.

Modules:
- `persist_data` — `Snapshot`, `ResetHistoryEntry`, `AppStateLabel`,
  `Heartbeats`, `MSG_BUF_SIZE`, `RING_SIZE`. The pico's `/health` endpoint
  serves `Snapshot` directly (its derived `Serialize` impl *is* the
  response schema).
- `reset` — `ResetReason` enum + `name()`, `INIT_STAGE_*` consts,
  `init_stage_name`, `classify_reset` (pure function injected with
  `WATCHDOG.REASON` bits by the bin).
- `fsm` — `app_state_name()` lookup co-located with the `AppState`
  discriminant table.
- `scheduler` — `EventQueue` driving the pico's poll cadence.
- `encoder` — QEM quadrature decode + accumulator.
- `api` — server URL normalization helper.

Features: `defmt` (enables `defmt::Format` derives), `serde` (enables
`Serialize` derives on persist-data types so the bin's `/health` handler
can serve `read_live()` with no intermediate response struct).

### `anova-oven-server`

Axum HTTP server. Owns all upstream credentials and connections. The runtime
is a **processor model**: each processor owns one external surface, holds
its own state, and exchanges typed commands/events with the other
processors over `tokio::sync::mpsc` channels.

**Credentials (env vars):**
- `ANOVA_EMAIL`    — Firebase email (required)
- `ANOVA_PASSWORD` — Firebase password (required)
- `ANOVA_TOKEN`    — optional static PAT for the Anova WebSocket API. When
  unset, the WebSocket authenticates with the Firebase ID token from the
  signed-in session and refreshes it automatically (~hourly), so WS auth is no
  longer a manual restart job. Set a PAT only if you want to pin a specific
  token.

**Optional env vars (defaults shown):**
- `ANOVA_BIND` — listen address (default `0.0.0.0:8080`)
- `ANOVA_WS_READ_TIMEOUT_SECS` — reconnect if no upstream frame arrives within
  this window, catching half-open sockets (default `1200`)
- `ANOVA_HTTP_TIMEOUT_SECS` — outbound HTTP timeout (default `10`)
- `ANOVA_HTTP_CONNECT_TIMEOUT_SECS` — outbound connect timeout (default `5`)
- `ANOVA_CURRENT_COOK_TIMEOUT_SECS` — current-cook query timeout (default `4`)
- `ANOVA_CURRENT_COOK_RESOLUTION_TIMEOUT_SECS` — per-document GET timeout
  while resolving the current cook (default `1`)
- `ANOVA_CURRENT_COOK_REFRESH_INTERVAL_SECS` — periodic current-cook refresh
  (default `60`)
- `ANOVA_RECIPES_REFRESH_INTERVAL_SECS` — periodic recipes refresh
  (default `3600`)
- `ANOVA_HISTORY_REFRESH_INTERVAL_SECS` — periodic history refresh
  (default `3600`)

**Running:**
```sh
ANOVA_EMAIL=you@example.com \
ANOVA_PASSWORD=secret \
cargo run -p anova-oven-server
# optionally pin a WebSocket PAT with ANOVA_TOKEN=anova-eyJ...
```

**Internal architecture:**
- `processors::ws` keeps a persistent WebSocket connection to
  `wss://devices.anovaculinary.io/`. On connect it parses
  `EVENT_APO_WIFI_LIST` to learn the cooker ID; subsequent
  `EVENT_APO_STATE` frames are translated into `WsEvent` values and
  forwarded to the state machine. Outbound `WsCommand`s (stop, start)
  are dispatched in the same loop, so command latency is bounded only
  by the current `stream.next()` poll. On disconnect it sleeps 5 s and
  reconnects indefinitely.
- `processors::firestore` owns the `reqwest` client, the cached Firebase
  session (ID token + refresh token), and all Firestore queries. It
  responds to typed `FirestoreCommand`s (fetch recipes, fetch history,
  fetch current cook, fetch single recipe). On `401` it automatically
  calls `refresh_session()` and retries — token expiry is no longer a
  manual restart job.
- `processors::state_machine` is the single source of truth. Inputs:
  `StateMachineCommand` from HTTP, `WsEvent` from the WebSocket,
  `FirestoreEvent` from Firestore, `Tick` from periodic refresh loops.
  Outputs: `WsCommand`, `FirestoreCommand`, and a `watch::Sender<ReadModel>`
  the HTTP layer reads through.
- `processors::http` runs the axum router and translates each request
  into a `StateMachineCommand`, awaiting the reply over a `oneshot`.
- `cook_progress::CookProgressTask` watches `(status, current_cook)`
  and derives `CookProgress` (current stage index, next-stage-ready
  flag, descriptions). `GET /status` stitches the latest `CookProgress`
  onto the `OvenStatus` before serializing.

**Task supervision:** the long-lived processors are spawned via
`spawn_critical`, which watches each task's `JoinHandle`. If any of them
exits — a clean return (an upstream channel closed) or a panic — the
supervisor logs the culprit and calls `std::process::exit(1)` so the OS
supervisor restarts the whole process from a clean slate, rather than
leaving a zombie where some processors serve stale data while a
load-bearing one is gone. Run under `systemd` with `Restart=always` (or an
equivalent container restart policy) to make this self-healing.

**Endpoints:**
- `GET /status`         — current `OvenStatus` (with derived
                          `cook_progress` inlined when cooking).
                          Returns 503 while the WebSocket is still
                          establishing.
- `GET /recipes`        — cached recipe list (own + bookmarked,
                          deduplicated by ID, own takes precedence).
- `POST /update-recipes`— force-refresh recipes from Firestore and
                          return the new list.
- `GET /history`        — cached cook history with resolved titles.
- `GET /current-cook`   — `CurrentCook` JSON, or 204 if none in progress.
- `POST /start`         — body `{ "recipe_id": "..." }`. Looks up the
                          recipe from the cache, sends `CMD_APO_START`
                          over the WebSocket, and seeds the cook-progress
                          tracker with the recipe stages so the next
                          `/status` already carries `cook_progress`.
                          Fire-and-forget `204` once queued.
- `POST /stop`          — sends `CMD_APO_STOP`; fire-and-forget `204`.

**Module layout:**
- `src/main.rs`              — entry point, channel wiring, tick loops
- `src/processors/ws.rs`     — WebSocket processor (connect, dispatch,
                               reconnect)
- `src/processors/firestore.rs` — Firestore processor (auth refresh, query
                               dispatch)
- `src/processors/state_machine.rs` — central decision logic
- `src/processors/http.rs`   — axum router + handlers
- `src/runtime/types.rs`     — `StateMachineCommand`/`Event`, `WsCommand`/
                               `Event`, `FirestoreCommand`/`Event`,
                               `TickKind`, `SmError`
- `src/protocol.rs`          — `EVENT_APO_STATE` → `OvenStatus`,
                               `EVENT_APO_WIFI_LIST` → cooker ID
- `src/firestore.rs`         — Firebase sign-in/refresh, runQuery, doc
                               GETs, mapping to `anova-oven-api` types
- `src/cook_progress.rs`     — `CookProgressTask` (derives `CookProgress`)
- `src/read_model.rs`        — `ReadModel` published by the state machine
- `src/recipe.rs`            — recipe helpers (preheat stage id rewrite)

**Dependencies:** axum 0.8, tokio 1 (full), reqwest 0.13 (json, rustls),
tokio-websockets 0.13 (client, native-tls, fastrand, openssl),
futures-util 0.3 (with `sink` feature for `SinkExt`/`split()`),
serde, serde_json 1.0, http 1, uuid 1 (v4 + serde),
tracing/tracing-subscriber/tracing-appender, `anova-oven-api`.

### `anova-oven-cli`

Desktop binary. Calls the local server. Uses `anova-oven-api` types for
deserialization.

**Subcommands:**
- `status`        — `GET /status`
- `recipes`       — `GET /recipes`
- `history`       — `GET /history`
- `current-cook`  — `GET /current-cook`
- `start --recipe-id <id>` — `POST /start`
- `stop`          — `POST /stop`

**Server address:** `--server <addr>` flag (default `http://localhost:8080`),
also `ANOVA_SERVER` env var. A bare `host:port` without `http://` is accepted
and has the scheme prepended automatically.

**Running:**
```sh
cargo run -p anova-oven-cli -- status
cargo run -p anova-oven-cli -- --server 10.0.1.42:8080 recipes
ANOVA_SERVER=10.0.1.42:8080 cargo run -p anova-oven-cli -- history
cargo run -p anova-oven-cli -- start --recipe-id <id>
cargo run -p anova-oven-cli -- stop
```

**Dependencies:** clap 4 (derive, env), reqwest, tokio 1, `anova-oven-api`.

### `anova-oven-pico`

Pico W firmware. Standalone workspace. Connects to the local server over
plain HTTP. Logs via defmt-rtt.

The firmware is a full appliance UI, not the "poll once and log" prototype
the early Phase-2 plan described:

- 16×2 HD44780 LCD (4-bit bus, async driver) showing status / cook
  progress / next-stage prompts / recovery messages.
- Rotary encoder + push button (input via `embassy-rp` GPIO).
- LED backlight on PWM with policies for full / dimmed states.
- FSM (in `state.rs`) selecting which view to display and when to issue
  Start/Stop commands.
- Polling client (`api_client.rs`) issuing `GET /status` at 1 Hz with
  tiered backoff under failure (5 s → 15 s → 30 s), `GET /current-cook`
  every 10 polls, and `GET /recipes` at startup + on demand.
- Persistent crash-recording region in `.uninit` SRAM
  (`PersistRegion` in `persist.rs`) carrying reset counters, panic
  message, heartbeats, free-heap watermark, and an 8-entry reset-reason
  ring. Decoded shapes live in `anova-oven-pico-core::persist_data`.
- Custom `#[panic_handler]` + HardFault exception that record into the
  persist region and `SCB::sys_reset()` (no `panic-probe`).
- Hardware watchdog (8 s timeout, 2 s feed interval) plus deadlines on
  WiFi join and DHCP, both attributed to `ResetReason::InitTimeout` if
  they exceed their bring-up budget.
- `picoserve 0.18` listening on port 80 with one route, `GET /health`,
  serving the live `Snapshot` JSON over the LAN — see
  [`docs/pico-crate-drift.md`](pico-crate-drift.md) for the
  zero-drift contract between `PersistRegion`, `Snapshot`, and
  `dump-persist.sh`.

**Build-time credentials (required env vars — injected via `env!()`):**

| Env var               | Example            | Purpose                          |
|-----------------------|--------------------|----------------------------------|
| `ANOVA_WIFI_SSID`     | `"MyNetwork"`      | WiFi network name                |
| `ANOVA_WIFI_PASSWORD` | `"hunter2"`        | WiFi password                    |
| `ANOVA_SERVER_URL`    | `"10.0.1.42:8080"` | Local server address             |

`ANOVA_SERVER_URL` may be given as a bare `host:port` or with `http://`;
`http://` is prepended automatically at runtime if absent.

**Building:**
```sh
cd crates/anova-oven-pico
ANOVA_WIFI_SSID="MyNetwork" \
ANOVA_WIFI_PASSWORD="hunter2" \
ANOVA_SERVER_URL="10.0.1.42:8080" \
cargo build --release
```

**Features:**
- `verbose-logs` — enables defmt on cyw43/cyw43-pio/embassy-net/reqwless
  and per-event INFO logs in the encoder/heap tasks. Off by default so the
  RTT buffer stays drainable when no probe is attached. Warnings and errors
  are always emitted.

**Dependencies:** embassy-executor 0.10, embassy-rp 0.10, embassy-net 0.9,
embassy-sync 0.8, embassy-time 0.5, embassy-futures 0.1, cyw43 0.7,
cyw43-pio 0.10, cortex-m, cortex-m-rt, defmt, defmt-rtt 1.1,
embedded-alloc 0.7, embedded-io-async 0.7,
reqwless 0.14 (plain HTTP, no TLS feature),
picoserve 0.18 (embassy, json),
hd44780-driver (git, async),
serde_json (no_std + alloc),
heapless 0.9 (serde),
static_cell 2,
portable-atomic 1 (critical-section),
portable-atomic-util 0.2 (alloc),
`anova-oven-api` (no_std), `anova-oven-pico-core` (defmt + serde).

---

## Implementation Plan

### Phase 1 — COMPLETED (superseded)

The initial direct-to-Firebase architecture validated the upstream protocols
and proved the TLS 1.2 blocker. Code from that phase is the basis for the
server's internal implementation.

Key findings carried forward:
- Firebase sign-in flow and Firestore `runQuery` shape (security rules
  require `userProfileRef == doc("user-profiles", uid)` + `draft == false`).
- `EVENT_APO_STATE` parse logic.
- Pico W embassy/cyw43 bring-up (WiFi, DHCP, DNS, TCP).

### Phase 2 — COMPLETED

- ✅ `anova-oven-api` crate — `no_std + alloc`, shared types, serde
  round-trip tests
- ✅ `anova-oven-server` crate — axum server (now factored into the
  processor model described above), persistent WebSocket with
  auto-reconnect, Firestore client with auto-refresh on 401, in-memory
  caching with periodic refresh ticks
- ✅ `anova-oven-cli` — thin HTTP client, 6 subcommands, `--server`
  flag with automatic scheme prepending
- ✅ `anova-oven-pico` — full appliance UI (LCD/encoder/button), plain
  HTTP to local server, WiFi/SSID/server URL injected via `env!()` at
  compile time, persistent crash-recording region, `/health` server,
  watchdog
- ✅ `anova-oven-pico-core` — host-testable pure-logic crate carved out
  of the firmware (FSM/persist/reset/scheduler/encoder/api logic)
- ✅ `anova-oven-protocol` and `anova-oven-firestore` deleted from
  workspace (absorbed into `anova-oven-server`)

### Phase 3 — Cook Commands

- ✅ **`POST /stop`** — implemented. `CMD_APO_STOP` is sent
  fire-and-forget over the WebSocket. CLI `stop` subcommand wired.
- ✅ **`POST /start`** — implemented. Body `{ "recipe_id": "..." }`,
  recipe stages looked up from the cache, `CMD_APO_START` frame built,
  cook-progress tracker seeded so the immediately-following `/status`
  carries `cook_progress`.
- ⚠️ **`CMD_APO_START_STAGE`** — removed. The oven backend rejects all
  start-stage commands with "unauthorized" regardless of payload shape;
  see [`docs/exploration/start-stage-unauthorized-debug.md`](exploration/start-stage-unauthorized-debug.md).
  The pico/server now surface `next_stage_ready` in `CookProgress` and
  rely on the phone app to advance.

### Phase 4 — Usability

- **Server:** exponential backoff on WebSocket reconnect (current is a
  flat 5 s sleep), graceful SIGINT shutdown. Token-expiry handling
  already lands automatically via `maybe_refresh_session` on 401.
- **CLI:** richer output formatting (tables, colours), `--watch` flag,
  `--json` flag.
- **Pico:** OTA updates (see [`docs/pico-ota.md`](pico-ota.md)),
  transport security (see [`docs/pico-transport-security.md`](pico-transport-security.md)),
  larger LCD, button-driven recipe selection (today the pico can start
  a recipe but the picker UX is minimal).

### Phase 5 — Production hardening

Tracked in [`docs/pico-review.md`](pico-review.md) §5 (no tests, no CI
gate, no resource budget, no observability/update path, implicit safety
case, undecided security posture, no firmware provenance, no
error-handling policy).

---

## Known Gaps and Gotchas

- **Recipe/history cache invalidation:** Recipes and history are fetched
  from Firestore at startup and then refreshed on the configured tick
  intervals (`ANOVA_RECIPES_REFRESH_INTERVAL_SECS`, default 1 h, etc.).
  `POST /update-recipes` forces an immediate refresh and returns the new
  list, so the pico/CLI can show edits without waiting for the tick.

- **WebSocket reconnect backoff:** The reconnect loop in
  `processors/ws.rs` sleeps a flat 5 s. Phase 4 should replace with
  exponential backoff (e.g. 1 s → 2 s → 4 s → … → 60 s).

- **Start/stop are fire-and-forget:** the HTTP handlers return as soon
  as the command is queued; they do not wait for the oven's
  `RESPONSE { status: "ok" }` frame. The matching infrastructure
  (`HashMap<Uuid, oneshot::Sender<_>>` of pending requests) is not yet
  built. Clients should poll `/status` to confirm the new mode.

- **Cooker ID availability:** the cooker ID only arrives in
  `EVENT_APO_WIFI_LIST`, the first message the server receives after
  connecting. Until that message is processed, write endpoints return
  HTTP 503. In practice this window is well under a second.

- **Bookmarked recipes are N+1 fetches:** `GET /recipes` first queries
  `users/{uid}/favorite-oven-recipes` for `recipeRef` document references,
  then issues one individual GET per bookmark. There is no batch GET in
  the Firestore REST API. Results are cached, so subsequent reads are
  free until the next refresh tick.

- **`CMD_APO_START_STAGE` is rejected:** multi-stage cooks where later
  stages have `user_action_required == false` (i.e. would normally
  auto-advance) **will not auto-advance** through our software — the
  phone app must be used for every stage transition. See
  [`docs/exploration/start-stage-unauthorized-debug.md`](exploration/start-stage-unauthorized-debug.md).

- **Pico crash recovery is in-RAM:** the persist region lives in
  `.uninit` SRAM and survives every reset *except* a true power-cycle.
  Bumping `MAGIC` in `persist.rs` (e.g. after a layout change) also
  invalidates the region on the first boot after flashing. See
  [`docs/pico-crate-drift.md`](pico-crate-drift.md) and
  [`docs/pico-reset-button.md`](pico-reset-button.md).

---

## Reference Material

- WebSocket protocol: [`docs/oven-websocket-api.md`](oven-websocket-api.md)
- Cloud API (Firestore): [`docs/oven-cloud-api.md`](oven-cloud-api.md)
- Pico crate drift map: [`docs/pico-crate-drift.md`](pico-crate-drift.md)
- Pico OTA brief: [`docs/pico-ota.md`](pico-ota.md)
- Pico transport security brief: [`docs/pico-transport-security.md`](pico-transport-security.md)
- Pico reset-button note: [`docs/pico-reset-button.md`](pico-reset-button.md)
- Pico crate review: [`docs/pico-review.md`](pico-review.md)
- Exploration / debugging archive: [`docs/exploration/`](exploration/)
- Community protocol docs (Go client): `../anova-oven-api/`
- Official developer docs + PAT management: https://developer.anovaculinary.com/
- Official reference implementation: https://github.com/anova-culinary/developer-project-wifi
