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
│  axum HTTP API under /api  (plain HTTP, local network)   │
│  ├── GET  /api/status         → oven state JSON          │
│  ├── GET  /api/recipes        → recipe list JSON         │
│  ├── POST/PATCH/DELETE /api/recipes[/{id}] → recipe CRUD │
│  ├── POST /api/update-recipes → refresh + return list    │
│  ├── GET  /api/history        → cook history JSON        │
│  ├── GET  /api/current-cook   → in-progress cook or 204  │
│  ├── POST /api/start          → start cook from recipe   │
│  ├── POST /api/stop           → CMD_APO_STOP             │
│  └── …controls (local mode)                              │
│  Web UI: /  (status, light)   /recipes  (recipe editor)  │
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
  │  Display +      │         │                     │
  │  encoder        │         │                     │
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
- `button` — integrating debouncer for the encoder's push button (rejects the
  noise the shaft couples onto the switch line while rotating).
- `api` — server URL normalization helper.

Features: `defmt` (enables `defmt::Format` derives), `serde` (enables
`Serialize` derives on persist-data types so the bin's `/health` handler
can serve `read_live()` with no intermediate response struct).

### `anova-oven-server`

Axum HTTP server. Owns all upstream credentials and connections. The runtime
is a **processor model**: each processor owns one external surface, holds
its own state, and exchanges typed commands/events with the other
processors over `tokio::sync::mpsc` channels.

**Mode:** `ANOVA_MODE=cloud` (the default) follows the oven through Anova's
cloud, as described below. `ANOVA_MODE=local` stands in for the oven's own
cloud instead (see [oven-offline-plan.md](oven-offline-plan.md)): it listens
for the oven on 443, and needs no credentials and no internet. Recipes,
history and the current cook are files under `ANOVA_DATA_DIR`; fill the
recipes once with `anova-oven-server export-recipes` while the internet is
still available.

**Credentials (env vars, cloud mode):**
- `ANOVA_EMAIL`    — Firebase email (required)
- `ANOVA_PASSWORD` — Firebase password (required)
- `ANOVA_TOKEN`    — optional static PAT for the Anova WebSocket API. When
  unset, the WebSocket authenticates with the Firebase ID token from the
  signed-in session and refreshes it automatically (~hourly), so WS auth is no
  longer a manual restart job. Set a PAT only if you want to pin a specific
  token. (The Firebase ID token authenticating the WS was verified
  empirically 2026-07-09 — both a PAT and a fresh id_token receive
  `EVENT_APO_WIFI_LIST`; the WS accepts either for the same account. Note the
  server enforces the token *after* the HTTP 101 upgrade, closing the socket
  silently on a bad token rather than failing the handshake.)

**Optional env vars (defaults shown):**
- `ANOVA_BIND` — listen address (default `0.0.0.0:8080`)
- `ANOVA_WS_READ_TIMEOUT_SECS` — reconnect if no `EVENT_APO_STATE` frame
  arrives within this window, catching half-open sockets and quiet-but-stale
  connections. Anova emits state ~every 10 min idle / ~10s cooking, so the
  default `1200` is ~2x the idle heartbeat. Keepalive pings do not reset it.
  In local mode it drops the oven's connection instead, and defaults to `90`,
  since the server sets the oven's idle report rate to 30s.
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

**Local mode env vars (defaults shown):**
- `ANOVA_DEVICE_BIND` — where the oven connects (default `0.0.0.0:443`).
  Point `iot-prod.anovaculinary.io` at this host in the LAN's DNS (see
  [oven-cloud-emulation.md](oven-cloud-emulation.md#redirecting-the-oven)).
- `ANOVA_TIMEZONE` — IANA zone answered to the oven's `timezone` requests,
  e.g. `America/Los_Angeles`. Defaults to the system's zone; if neither is
  usable the requests go unanswered, which the oven tolerates.
- `ANOVA_HEALTHCHECK_INTERVAL_SECS` — `CMD_APO_HEALTHCHECK` cadence
  (default `10`, as the real cloud does)
- `ANOVA_DATA_DIR` — where the local store keeps its files (default
  `anova-oven-data`, relative to the working directory):
  - `recipes/*.json` — one `Recipe` per file, as `GET /recipes` serves
    them. Written by `export-recipes` and the recipe endpoints (the web UI's
    editor), found by the `id` inside rather than the file name, so a file
    added by hand works too. Re-read on every refresh and
    `POST /update-recipes`.
  - `active-cook.json` — a journal of the cook in flight, keyed by the
    oven's `cookId`: title, recipe ID, start time, and when the oven last
    reported it. Written on `/start` (or on first sight of a cook started
    from the oven's panel, as `[manual]`), rewritten in place on each report
    of that cook (every 5s while cooking), and removed when the cook goes
    to history. A restart mid-cook keeps the title. The stages come from the
    oven's own `cook` block.
  - `history.jsonl` — one `HistoryEntry` per line, one line per cook.
    `GET /history` returns the last 50, newest first. A cook goes to history
    when an oven report shows the journalled cook is no longer running: on
    the cooking-to-idle edge, on a start over a running cook, or on the
    first report after a restart if it ended while the server was down
    (dated by its last report).

**Web UI.** Static pages compiled into the binary
(`processors/http/ui/`), plain HTML and a little JavaScript with no build
step; they call `/api` from the browser. Temperatures show in °F or °C, picked
with the switch in the header (°F until changed, remembered per browser), and
are always sent as Celsius.

- `/` — status (temperatures, timer, steam, probe, door, water, light, link),
  the running cook's stage with stop and next-stage, and the light. While
  nothing is cooking, a manual cook: a title and stages in the same editor as
  recipes, started with `POST /start`.
- `/recipes` — the recipe list, own first then bookmarked, each by title,
  with a start button per recipe.
- `/recipes/new`, `/recipes/{id}` — the recipe editor. It works in *steps*,
  as Anova's app does: a cook stage, optionally with "Preheat first", which
  saves as a preheat stage with the same settings followed by the cook
  stage (the pairing the oven expects; see `recipe::rewrite_preheat_stage_ids`).
  Per step: title, mode (oven or sous vide), temperature, steam, heating
  elements (a radio of the six allowed combinations), whether it starts
  automatically (and, with a preheat, whether it carries on after
  preheating), and how it ends (a radio: until you move on, a timer or a
  probe). A preheat matters because the oven starts a stage's timer the
  moment the stage starts: without one, a timed stage counts down while the
  oven is still heating. Fan, vent and rack aren't shown:
  new stages get fan 100 and no vent or rack, and existing values are kept on
  save, as is anything else the form doesn't show. The page checks the
  same rules as the API as you type, and Save stays disabled until they pass.
  An existing recipe's page also has Start, which starts the saved recipe;
  it reads "Save first" and stays disabled while there are unsaved edits.

**Home Assistant over MQTT (off unless `ANOVA_MQTT_URL` is set):**
- `ANOVA_MQTT_URL` — e.g. `mqtt://broker.lan:1883` (port defaults to 1883;
  `mqtts://` isn't supported yet)
- `ANOVA_MQTT_USERNAME`, `ANOVA_MQTT_PASSWORD`
- `ANOVA_MQTT_TOPIC_PREFIX` — default `anova/oven`
- `ANOVA_MQTT_DISCOVERY_PREFIX` — default `homeassistant`

**Running:**
```sh
ANOVA_EMAIL=you@example.com \
ANOVA_PASSWORD=secret \
cargo run -p anova-oven-server
# optionally pin a WebSocket PAT with ANOVA_TOKEN=anova-eyJ...

# or, with no Anova account or internet. First, while online, copy the
# account's own and bookmarked recipes into ANOVA_DATA_DIR/recipes:
ANOVA_EMAIL=you@example.com ANOVA_PASSWORD=secret \
cargo run -p anova-oven-server -- export-recipes
# then:
ANOVA_MODE=local ANOVA_TIMEZONE=America/Los_Angeles \
cargo run -p anova-oven-server
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
- `processors::device` (local mode) replaces `processors::ws`. It accepts
  the oven's TLS connection with a self-signed RSA certificate generated at
  startup, reads `deviceId` from the upgrade's `Authorization` JWT (without
  verifying it) and emits it as `CookerDiscovered`, then forwards each
  `state` as `ApoState`. It sends `CMD_APO_SET_REPORT_STATE_RATE` on every
  connect (`{"idle": 30, "cooking": 5}`), `CMD_APO_HEALTHCHECK` every 10s, and answers `timezone`. One
  oven at a time: a new connection replaces the current one. Start and stop
  go out as `{id, type, payload}` with the request ID as `id`; when that ID
  shows up in a `state`'s `processedCommandIds` it emits
  `CommandAck { status: "ok" }`, and after 10s without it, `"timeout"`.
- `processors::firestore` owns the `reqwest` client, the cached Firebase
  session (ID token + refresh token), and all Firestore queries. It
  responds to typed `StoreCommand`s (fetch recipes, fetch history,
  fetch current cook, fetch single recipe). On `401` it automatically
  calls `refresh_session()` and retries — token expiry is no longer a
  manual restart job. In local mode `processors::local_store` answers the
  same commands from files under `ANOVA_DATA_DIR`. Two commands matter
  only to it: `FetchCurrentCook` carries the oven's own cook, which the
  local store builds the current cook from, and `OvenReported` (sent on
  every oven report except while a start is pending) drives its journal
  and history. Firestore ignores both.
- `processors::state_machine` is the single source of truth. Inputs:
  `StateMachineCommand` from HTTP, `WsEvent` from the WebSocket,
  `StoreEvent` from Firestore, `Tick` from periodic refresh loops.
  Outputs: `WsCommand`, `StoreCommand`, and a `watch::Sender<ReadModel>`
  the HTTP layer reads through.
- `processors::http` runs the axum router and translates each request
  into a `StateMachineCommand`, awaiting the reply over a `oneshot`.
- `processors::mqtt` (only with `ANOVA_MQTT_URL`) publishes the oven to
  Home Assistant through MQTT discovery and takes commands from it. It
  reads the same read model, cook progress and liveness the HTTP API
  serves, and sends the same `StateMachineCommand`s, so the two can't drift
  apart. Spawned outside `spawn_critical`: a broker outage only stops
  updates, and the client reconnects with backoff, republishing discovery
  and the retained topics on every connect. Topics under the prefix:
  `availability` (last-will), `state` (the `/status` JSON plus `connected`,
  `phase`, `recipe_title`, `timer_remaining_secs`, `stage_ends_at`),
  `next_stage/availability`, `recipes`, `recipe/selected` and its
  `/attributes`, `event` (milestones, not retained) and `cmd/<name>`
  (`start`, `stop`, `select_recipe`, and in local mode `next_stage`,
  `lamp`, `timer`, `temperature`). Milestones (`cook_started`,
  `preheat_done`, `stage_started`, `stage_waiting`, `cook_done`,
  `cook_stopped`, `oven_lost`) are a pure function of successive snapshots
  in `mqtt::milestones`, keyed by the oven's `cookId`; the first snapshot
  is a baseline, so a restart announces nothing. `command_failed` comes from
  refusals and from the state machine's failed acks (a `broadcast` of
  `CommandFailure`). The discovery set is pinned by
  `testdata/mqtt-discovery.json`; regenerate it with
  `UPDATE_SNAPSHOTS=1 cargo test -p anova-oven-server discovery` after
  checking no `unique_id` changed.
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

**Logging what the oven does.** Some of what the oven does isn't in any
recipe: the lamp has no per-stage setting (only a preference and
`CMD_APO_SET_LAMP`), and the firmware sets the fan and vent itself at the end
of a cook. To explain such changes, `oven_log::OvenLog` (in both modes) logs,
at `info`:

- `[oven] outputs` on the first oven report, then `[oven] outputs changed`
  whenever the lamp, lamp preference, fan speed or vent changes, with the
  previous values and the active stage. Heating elements are left out, since
  the oven cycles them constantly to hold a temperature.
- `[oven] stage started` as each stage becomes active, with the oven's own
  copy of its settings: title, kind, mode, temperature, elements, fan, vent,
  steam, timer and probe.

So `journalctl -u anova-oven-server | grep '\[oven\]'` gives the timeline of a
cook. For anything the server can't see (e.g. why the firmware switched
something), the ESP32 console log from the Pi bridge
([oven-uart.md](oven-uart.md)) has the firmware's own account.

**Endpoints.** All JSON endpoints live under `/api`, leaving the other
paths to the web UI. The list below and the rest of this document give them
without the prefix: `GET /status` is `GET /api/status`. (The pico's own
`/health` server, further down, is a different server and has no prefix.)

- `GET /status`         — current `OvenStatus` (with derived
                          `cook_progress` inlined when cooking).
                          Returns 503 while the WebSocket is still
                          establishing. Also carries an `upstream`
                          object `{ connected, disconnected_secs }` so a
                          display client can tell a fresh reading from a
                          cached one served while the Anova link is down
                          (the poll is `200` either way). Same source as
                          `/health`; see "Upstream staleness on the pico".
- `GET /recipes`        — cached recipe list (own + bookmarked,
                          deduplicated by ID, own takes precedence).
- `POST /update-recipes`— force-refresh recipes from Firestore and
                          return the new list.
- `GET /recipes/{id}`   — local mode: one recipe from the files.
- `POST /recipes`       — local mode, body `{ "title", "stages": [Stage] }`:
                          create a recipe of your own under a new ID.
                          `201` with the saved recipe.
- `PATCH /recipes/{id}` — local mode, body with `title` and/or `stages`:
                          replace those fields. `200` with the saved recipe.
- `DELETE /recipes/{id}`— local mode: remove the recipe's file. `204`.

                          Writes check that the oven could run the recipe
                          and answer `422` with the reason if not: stage
                          kinds; 25–250 °C, or 25–100 °C in sous vide (wet
                          bulb); steam and fan 0–100; rack 1–5; one or two
                          heating elements, never all three; a timer or a
                          probe, not both, a timer with
                          a duration and a probe with a 1–100 °C target; no
                          timer or probe on a preheat. The temperature,
                          element and probe limits are from Anova's
                          [oven-commands docs](https://developer.anovaculinary.com/docs/devices/wifi/oven-commands). Stages without an
                          `id` get one, the fan goes to 100 on a stage with
                          the rear element or steam
                          (`Stage::normalize_fan_speed`; the firmware logs
                          `Fan speed of 0% is not valid (rear element is
                          on)` otherwise), and `stage_count` is recomputed.
                          They go straight to the recipe files, then ask the
                          state machine to re-read them so `/start`, the
                          panels and Home Assistant pick the change up. In
                          local mode `GET /recipes` also reads the files
                          directly. All four answer `501` in cloud mode,
                          where recipes are Anova's.
- `GET /history`        — cached cook history with resolved titles.
- `GET /current-cook`   — `CurrentCook` JSON, or 204 if none in progress.
- `POST /start`         — body `{ "recipe_id": "..." }`, or a manual cook
                          `{ "title": "...", "stages": [Stage] }` (title
                          optional, "Manual cook" by default; the stages
                          are checked like a saved recipe's, `422` if the
                          oven couldn't run them, and the cook is journalled
                          and goes to history under its title). Looks up the
                          recipe from the cache, sends `CMD_APO_START`
                          over the WebSocket, and seeds the cook-progress
                          tracker with the recipe stages so the next
                          `/status` already carries `cook_progress`.
                          Fire-and-forget `204` once queued.
- `POST /stop`          — sends `CMD_APO_STOP`; fire-and-forget `204`.
                          In both modes the oven keeps holding the last
                          stage when its timer ends; nothing stops it but
                          `/stop` or the oven's panel. In local mode
                          `/status` then has `cook_done: true`.
- `POST /next-stage`    — local mode: `CMD_APO_START_STAGE` with the next
                          stage's id, whether or not the oven is waiting.
                          `/status` has `stage_transition_pending_user_action`
                          (and `cook_progress.next_stage_ready`) when it is.
- `POST /timer`         — local mode, body `{ "seconds": 90 }`: restart the
                          current stage's countdown.
- `POST /temperature`   — local mode, body `{ "celsius": 180 }` (25–250):
                          the current stage's setpoint, in its own bulb mode.
- `POST /lamp`          — local mode, body `{ "on": true }`. The oven only
                          acts on it mid-cook.
- `POST /temperature-unit` — local mode, body `{ "unit": "C" }` or `"F"`:
                          the oven's display unit.

                          All five answer `204` once sent (the ack is
                          tracked, not awaited), `409` if the oven's state
                          doesn't allow it (not cooking, no next stage) and
                          `501` in cloud mode. During a preheat, `/timer`
                          and `/temperature` go out as
                          `CMD_APO_UPDATE_COOK_STAGES`, since the oven
                          copies a preheat's `SET_*` onto the cook stage
                          after it.
- `GET /health`         — liveness probe. Always `200`; body is
                          `{ connected, seconds_since_last_state,
                          read_timeout_secs }`. `connected` is the upstream
                          WebSocket status; `seconds_since_last_state` climbs
                          when Anova goes quiet (`null` until the first frame).
                          Point an external monitor at this to catch silent
                          staleness the in-process watchdog can't (e.g. a
                          wedged process). Alert on `!connected` or
                          `seconds_since_last_state` approaching
                          `read_timeout_secs`.

**Upstream staleness on the pico:** the pico is the only always-on watcher
of the server, but it can't detect a stale Anova link on its own — a cached
`/status` is still a `200`, so its poll succeeds and it stays "happy." So the
server tells it: `GET /status` carries `upstream { connected, disconnected_secs }`,
populated from the same `Liveness` tracker `/health` uses. The pico piggybacks
on its existing 1 Hz `/status` poll (no second endpoint) and, when the link has
been down past a 60 s grace (`UPSTREAM_STALE_GRACE_SECS` — long enough to ignore
the routine ~5 s max-connection-age reconnects), takes over the LCD with an
"Anova Link Down / Stale Nm" warning (`AppState::UpstreamStale`). This is
distinct from the pico's existing `Offline` screen, which is about the pico's
own link to the *server* failing; upstream-stale means the server is reachable
but its link to Anova is down. The trigger is connection state, not data age —
idle state only arrives ~every 10 min, so "age" is naturally high and useless as
a staleness signal.

**Module layout:**
- `src/main.rs`              — entry point, channel wiring, tick loops
- `src/processors/ws.rs`     — WebSocket processor (connect, dispatch,
                               reconnect)
- `src/processors/device.rs` — device-side processor for local mode (TLS,
                               session duties, oven state)
- `src/processors/firestore.rs` — Firestore processor (auth refresh, query
                               dispatch)
- `src/processors/local_store.rs` — file-based store for local mode
                               (recipes, history, current cook)
- `src/processors/mqtt.rs`   — Home Assistant over MQTT (connection,
                               retained state, commands); `mqtt/discovery.rs`,
                               `mqtt/milestones.rs`, `mqtt/recipes.rs` are
                               pure and unit-tested
- `src/processors/state_machine.rs` — central decision logic
- `src/processors/http.rs`   — axum router + handlers
- `src/runtime/types.rs`     — `StateMachineCommand`/`Event`, `WsCommand`/
                               `Event`, `StoreCommand`/`Event`,
                               `TickKind`, `SmError`
- `src/protocol/mod.rs`      — `OvenState` (shared by both sides) →
                               `OvenStatus`
- `src/protocol/app.rs`      — app-side frames: `EVENT_APO_STATE`,
                               `EVENT_APO_WIFI_LIST` → cooker ID
- `src/protocol/device.rs`   — device-side frames (`{action, data}`)
- `src/protocol/stages.rs`   — stage JSON for start/update commands,
                               without the envelope
- `src/firestore.rs`         — Firebase sign-in/refresh, runQuery, doc
                               GETs, mapping to `anova-oven-api` types
- `src/cook_progress.rs`     — `CookProgressTask` (derives `CookProgress`)
- `src/read_model.rs`        — `ReadModel` published by the state machine
- `src/recipe.rs`            — recipe helpers (preheat stage id rewrite)

**Dependencies:** axum 0.8, tokio 1 (full), reqwest 0.13 (json, rustls),
tokio-websockets 0.13 (client, server, native-tls, fastrand, openssl),
tokio-rustls 0.26, rcgen 0.14 (aws_lc_rs, for RSA key generation),
jiff 0.2 (time zones), base64,
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
- `update-recipes` — `POST /update-recipes`; in local mode, re-reads the
  recipes directory after an edit
- Local mode only:
  - `next-stage`  — `POST /next-stage`
  - `timer <duration>` — `POST /timer`; `90s`, `45m`, `1h30m`, `1:30:00`
  - `temperature <temp>` — `POST /temperature`; with its unit, `180c` or
    `356f`, since the CLI otherwise shows °F
  - `lamp on|off` — `POST /lamp`
  - `temperature-unit c|f` — `POST /temperature-unit`

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

- One of four display backends, selected at build time by a `ui-*`
  Cargo feature (or none, for a headless build): a 16×2 HD44780
  character LCD, a 2.7" 400×240 Sharp Memory Display, a 2.42" 128×64
  SSD1305/SSD1309 OLED, or a 2.0" 320×240 ST7789 colour IPS TFT — all
  showing status / cook progress / next-stage prompts / recovery
  messages. The three graphical panels share their layout code, all
  three driving a 1-bit `DrawTarget` (the TFT expands to RGB565 at
  flush time, since a full colour framebuffer would not fit in SRAM);
  the layout decisions live in `anova-oven-pico-core::view_plan` and
  are host-tested. Wiring and per-panel notes:
  [`docs/pico-displays.md`](pico-displays.md).
- Rotary encoder + push button (input via `embassy-rp` GPIO). The button is
  sampled and integrated rather than edge-triggered — see §1.6 of
  [`docs/pico-review.md`](pico-review.md).
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
  built. Clients should poll `/status` to confirm the new mode. In local
  mode the state machine does get a real ack or a timeout from the oven,
  and rolls back on failure, but the HTTP reply still doesn't wait for it.

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
- Pico display backends + wiring: [`docs/pico-displays.md`](pico-displays.md)
- Pico crate drift map: [`docs/pico-crate-drift.md`](pico-crate-drift.md)
- Pico OTA brief: [`docs/pico-ota.md`](pico-ota.md)
- Pico transport security brief: [`docs/pico-transport-security.md`](pico-transport-security.md)
- Pico reset-button note: [`docs/pico-reset-button.md`](pico-reset-button.md)
- Display SPI / DMA investigation: [`docs/pico-display-dma.md`](pico-display-dma.md)
- Pico crate review: [`docs/pico-review.md`](pico-review.md)
- Exploration / debugging archive: [`docs/exploration/`](exploration/)
- Community protocol docs (Go client): `../anova-oven-api/`
- Official developer docs + PAT management: https://developer.anovaculinary.com/
- Official reference implementation: https://github.com/anova-culinary/developer-project-wifi
