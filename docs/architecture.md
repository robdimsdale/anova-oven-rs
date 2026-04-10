# Architecture and Implementation Plan

## Project Goal

Build a local server, a Rust CLI, and Raspberry Pi Pico W firmware to control
an Anova Precision Oven v1 as a replacement for the Anova mobile app.

Target capabilities:
- Read oven state (temperature, heating elements, steam, timer, etc.)
- List user recipes and cook history
- Send cook commands (`CMD_APO_START`, `CMD_APO_STOP`, etc.)

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────┐
│  anova-oven-server  (Linux/macOS, full std)          │
│                                                      │
│  axum HTTP API  (plain HTTP, local network)          │
│  ├── GET  /status     → simplified oven state JSON   │
│  ├── GET  /recipes    → simplified recipe list JSON  │
│  ├── GET  /history    → simplified cook history JSON │
│  └── POST /stop       → send CMD_APO_STOP            │
│                                                      │
│  Internal (hidden from clients):                     │
│  ├── persistent WebSocket → devices.anovaculinary.io │
│  │     (TLS 1.2 via tokio-websockets + native-tls)   │
│  └── Firestore client (reqwest) → Firebase           │
└───────┬────────────────────────────────────┬─────────┘
        │ plain HTTP, local network (no TLS) │
  ┌─────┴───────────┐         ┌──────────────┴──────┐
  │  anova-oven-    │         │  anova-oven-cli     │
  │  pico           │         │                     │
  │                 │         │  Desktop binary.    │
  │  Pico W target. │         │  Prototypes the     │
  │  Connects to    │         │  embedded UI/UX     │
  │  server over    │         │  against the same   │
  │  plain HTTP     │         │  server API.        │
  │  (embassy-net,  │         │  Uses the same      │
  │  no TLS).       │         │  anova-oven-api     │
  │  Uses defmt to  │         │  types.             │
  │  log state.     │         │                     │
  └─────────────────┘         └─────────────────────┘
           │     crate dependency      │
           └──────────┬────────────────┘
                      │
           ┌──────────▼──────────┐
           │  anova-oven-api     │
           │  (no_std + alloc)   │
           │                     │
           │  Shared request /   │
           │  response types     │
           │  for the local      │
           │  server API.        │
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
the server where there are no meaningful limits. The Pico W becomes a thin
client over plain HTTP on the local network.

The CLI is intentionally structured to mirror the embedded client — it uses the
same `anova-oven-api` types and the same HTTP calls to the same server — so
that UI/UX can be prototyped quickly on the desktop before being ported to the
Pico.

---

## Crate Structure

```
crates/
  anova-oven-api/      # no_std + alloc — shared types for the local server API
  anova-oven-server/   # std — local HTTP server (axum, tokio, reqwest)
  anova-oven-cli/      # std — desktop CLI binary
  anova-oven-pico/     # no_std — Pico W firmware (standalone workspace)
```

The old `anova-oven-protocol` and `anova-oven-firestore` crates have been
absorbed into `anova-oven-server` and deleted.

### `anova-oven-api` (no_std + alloc)

Defines the simplified JSON types served by `anova-oven-server` and consumed
by both `anova-oven-cli` and `anova-oven-pico`. No knowledge of WebSockets,
Firebase, or Firestore.

**Types:**

```rust
// GET /status
pub struct OvenStatus {
    pub mode: String,              // "idle" | "cook" | "preheat"
    pub temperature_c: f32,        // current dry-bulb celsius
    pub target_temperature_c: Option<f32>,
    pub timer_current_secs: u64,
    pub timer_total_secs: u64,
    pub steam_pct: f32,            // 0–100
    pub door_open: bool,
    pub water_tank_empty: bool,
}

// GET /recipes  → Vec<Recipe>
pub struct Recipe {
    pub id: String,
    pub title: String,
    pub stage_count: usize,
    pub stages: Vec<Stage>,
}

pub struct Stage {
    pub kind: String,              // "preheat" | "cook"
    pub temperature_c: f32,
    pub duration_secs: Option<u64>,
    pub steam_pct: f32,
    pub fan_speed: u8,
}

// GET /history  → Vec<HistoryEntry>
pub struct HistoryEntry {
    pub recipe_title: String,
    pub ended_at: String,          // ISO 8601
    pub stage_count: usize,
}
```

All types derive `serde::Serialize` + `serde::Deserialize` with
`default-features = false` so they compile for `thumbv6m-none-eabi`.

**Key design decisions:**
- Temperatures are always Celsius; the CLI/Pico can convert for display.
- No `draft`, `published`, `userProfileRef`, or other Firestore metadata.
- Stages are pre-filtered to `stepType == "stage"` (directions stripped).
- `stage_count` is included at the top level of `Recipe` for list views that
  don't need to decode the full `stages` array.
- History entries resolve recipe titles server-side; `"[custom]"` is used when
  a recipe document can't be fetched.

### `anova-oven-server`

Axum HTTP server. Owns all upstream credentials and connections.

**Credentials (env vars, required — no fallback):**
- `ANOVA_TOKEN`    — PAT token for the Anova WebSocket API
- `ANOVA_EMAIL`    — Firebase email
- `ANOVA_PASSWORD` — Firebase password

**Optional env vars:**
- `ANOVA_BIND` — listen address (default `0.0.0.0:8080`)

**Running:**
```sh
ANOVA_TOKEN=anova-eyJ... \
ANOVA_EMAIL=you@example.com \
ANOVA_PASSWORD=secret \
cargo run -p anova-oven-server
```

**Internal state:**
- A background tokio task maintains a persistent WebSocket connection to
  `wss://devices.anovaculinary.io/`. It is split into read and write halves
  (via `futures_util::StreamExt::split`) so that `tokio::select!` can
  concurrently receive incoming events and dispatch outgoing commands.
- On connect the server receives `EVENT_APO_WIFI_LIST` and caches the cooker
  ID in a `tokio::sync::watch` channel. This ID is required to address any
  outbound command frame. Commands issued before the first `EVENT_APO_WIFI_LIST`
  is received return HTTP 503.
- Outgoing commands are queued via a `tokio::sync::mpsc` channel
  (`capacity = 8`) from the HTTP handlers into the WebSocket task. The task
  reads commands in the same `select!` loop as incoming events, so command
  latency is bounded only by how long the current `stream.next()` poll takes
  (≤ 2 s during a cook, ≤ 30 s idle).
- On disconnect the task sleeps 5 s and reconnects indefinitely.
- A `reqwest` client handles Firebase sign-in and Firestore `runQuery`
  requests. The Firebase session (ID token + refresh token) is cached in
  memory (not on disk).
- Recipe and history data is fetched from Firestore on first request and
  cached in memory for the lifetime of the process.

**Endpoints:**
- `GET /status`  — reads from the `watch` channel, maps to `OvenStatus`.
  Returns HTTP 503 while the WebSocket connection is still establishing.
- `GET /recipes` — queries Firestore `oven-recipes` for the user's non-draft
  recipes, then also fetches bookmarked recipes from
  `users/{uid}/favorite-oven-recipes` and merges the two lists into a single
  `Vec<Recipe>`, deduplicated by ID (own recipes take precedence). Returns the
  combined list — clients see one flat list with no bookmark/own distinction.
- `GET /history` — queries `users/{uid}/oven-cooks`, resolves recipe
  titles via individual document GETs, maps to `Vec<HistoryEntry>`.
- `POST /stop`   — enqueues a `CMD_APO_STOP` onto the WebSocket task's command
  channel. Fire-and-forget: returns `204 No Content` once queued, or `503` if
  the cooker ID is not yet known. Clients should poll `GET /status` to confirm
  the oven reached mode `"idle"`.

**Module layout:**
- `src/main.rs`     — entry point, `AppState`, axum setup, WebSocket task
                      (including `WsCommand` enum, mpsc/watch channels,
                      `stop_command_json`), route handlers
- `src/protocol.rs` — Anova WebSocket message parsing (`EVENT_APO_STATE` →
                      `OvenStatus`, `EVENT_APO_WIFI_LIST` → cooker ID);
                      absorbed from old `anova-oven-protocol`
- `src/firestore.rs`— Firebase auth (sign-in, token refresh), Firestore
                      `runQuery` + document GET, Firestore Value unwrapping,
                      mapping to `anova-oven-api` types; absorbed from old
                      `anova-oven-firestore`

**Dependencies:** axum 0.8, tokio 1 (full), reqwest 0.13 (rustls),
tokio-websockets 0.13 (native-tls, fastrand, openssl),
futures-util 0.3 (**sink feature required** for `SinkExt` + `split()`),
serde, serde_json 1.0, http 1, uuid 1 (v4 + serde), `anova-oven-api`.

### `anova-oven-cli`

Desktop binary. Calls the local server. Uses `anova-oven-api` types for
deserialization. Mirrors the embedded client's data flow to validate UI/UX.

**Subcommands:**
- `status`  — `GET /status`, prints human-readable oven state
- `recipes` — `GET /recipes`, lists available recipes (own + bookmarked)
- `history` — `GET /history`, shows recent cooks
- `stop`    — `POST /stop`, sends stop command; polls no further (fire-and-forget)

**Server address:** `--server <addr>` flag (default `http://localhost:8080`),
also `ANOVA_SERVER` env var. A bare `host:port` without `http://` is accepted
and has the scheme prepended automatically.

**Running:**
```sh
cargo run -p anova-oven-cli -- status
cargo run -p anova-oven-cli -- --server 10.0.1.42:8080 recipes
ANOVA_SERVER=10.0.1.42:8080 cargo run -p anova-oven-cli -- history
cargo run -p anova-oven-cli -- stop
```

**Dependencies:** clap 4 (derive, env), reqwest 0.13, tokio 1, serde_json,
`anova-oven-api`.

### `anova-oven-pico`

Pico W firmware. Standalone workspace (avoids `critical-section` conflicts).
Connects to the local server over plain HTTP (no TLS, no Firebase, no
Firestore). Logs via defmt.

**Flow:**
1. WiFi + DHCP
2. `GET /recipes` — log recipe list via defmt (once on startup)
3. `GET /status` — log oven state via defmt
4. Poll `/status` every 10 s

**Build-time credentials (required env vars — injected via `env!()`):**

| Env var             | Example              | Purpose                          |
|---------------------|----------------------|----------------------------------|
| `ANOVA_WIFI_SSID`   | `"MyNetwork"`        | WiFi network name                |
| `ANOVA_WIFI_PASSWORD` | `"hunter2"`        | WiFi password                    |
| `ANOVA_SERVER_URL`  | `"10.0.1.42:8080"`   | Local server address             |

`ANOVA_SERVER_URL` may be given as a bare `host:port` or with `http://`;
`http://` is prepended automatically at runtime if absent. Compilation
fails with a clear error if any of the three vars are unset.

**Building:**
```sh
cd crates/anova-oven-pico
ANOVA_WIFI_SSID="MyNetwork" \
ANOVA_WIFI_PASSWORD="hunter2" \
ANOVA_SERVER_URL="10.0.1.42:8080" \
cargo build --release
```

**Dependencies:** embassy-executor 0.10, embassy-rp 0.10, embassy-net 0.9,
embassy-time 0.5, cyw43 0.7, cyw43-pio 0.10, cortex-m, cortex-m-rt, defmt,
panic-probe, embedded-alloc 0.6, embedded-io-async 0.7,
reqwless 0.14 (plain HTTP, no TLS feature),
serde_json 1.0 (no_std + alloc), `anova-oven-api` (no_std).

---

## Implementation Plan

### Phase 1 — COMPLETED (superseded)

The initial direct-to-Firebase architecture validated the upstream protocols
and proved the TLS 1.2 blocker. Code from that phase is the basis for the
server's internal implementation.

Key findings carried forward:
- Firebase sign-in flow and Firestore `runQuery` shape (exact filter required
  by security rules: `userProfileRef == doc("user-profiles", uid)` +
  `draft == false`).
- `anova-oven-protocol` parse logic for `EVENT_APO_STATE`.
- Pico W embassy/cyw43 bring-up (WiFi, DHCP, DNS, TCP).

### Phase 2 — COMPLETED

All five steps are done and compiling:

- ✅ **Step 1:** `anova-oven-api` crate — `no_std + alloc`, shared types,
  serde round-trip tests
- ✅ **Step 2:** `anova-oven-server` crate — axum server, WebSocket background
  task with auto-reconnect, Firestore client, in-memory caching
- ✅ **Step 3:** `anova-oven-cli` rewritten — thin HTTP client, 3 subcommands,
  `--server` flag with automatic scheme prepending
- ✅ **Step 4:** `anova-oven-pico` rewritten — plain HTTP to local server,
  WiFi/SSID/server URL injected via `env!()` at compile time
- ✅ **Step 5:** `anova-oven-protocol` and `anova-oven-firestore` deleted from
  workspace

### Phase 3 — Cook Commands (partially complete)

- ✅ **`POST /stop`** — implemented. `CMD_APO_STOP` is sent fire-and-forget
  over the WebSocket. CLI `stop` subcommand added. Pico support deferred
  (UI/UX not yet decided).
- ⬜ **`POST /start`** — not yet implemented. Requires a request body
  `{ "recipe_id": "..." }`, fetching the full recipe stages from the Firestore
  cache, and building a `CMD_APO_START` frame. Firestore recipe data is already
  in memory after the first `GET /recipes` call, so no additional fetch is
  needed if the cache is warm.

The mpsc command channel infrastructure from `POST /stop` is already in place;
`CMD_APO_START` just needs a new `WsCommand::Start { stages: Vec<...> }` variant
and a corresponding HTTP handler.

### Phase 4 — Usability

- **Server:** exponential backoff on WebSocket reconnect, graceful SIGINT
  shutdown, `?force_refresh` query param to bust the recipe/history cache,
  token refresh when Firebase ID token expires (currently signs in once and
  holds the session for the process lifetime — Firebase tokens expire after
  1 hour).
- **CLI:** richer output formatting (tables, colours), `--watch` flag for
  live status polling, machine-readable `--json` flag.
- **Pico:** LCD display (16×2 or 20×4, TBD), button input for recipe
  selection and start/stop.

---

## Key Dependencies

**`anova-oven-api`:**
- `serde` 1.0 (default-features=false, features: derive, alloc)
- `serde_json` 1.0 (default-features=false, features: alloc)

**`anova-oven-server`:**
- `axum` 0.8
- `tokio` 1 (full)
- `reqwest` 0.13 (json, rustls)
- `tokio-websockets` 0.13 (client, native-tls, fastrand, openssl)
- `futures-util` 0.3 (**features = ["sink"]** — required for `SinkExt` and `split()`)
- `serde`, `serde_json` 1.0
- `http` 1
- `uuid` 1 (v4, serde)
- `anova-oven-api`

**`anova-oven-cli`:**
- `clap` 4 (derive, env)
- `reqwest` 0.13 (json, rustls)
- `tokio` 1 (full)
- `serde_json` 1.0
- `anova-oven-api`

**`anova-oven-pico`:**
- `embassy-executor` 0.10, `embassy-rp` 0.10, `embassy-net` 0.9,
  `embassy-time` 0.5
- `cyw43` 0.7, `cyw43-pio` 0.10
- `cortex-m`, `cortex-m-rt`, `defmt`, `panic-probe`
- `embedded-alloc` 0.6
- `embedded-io-async` 0.7
- `reqwless` 0.14 (plain HTTP, no TLS feature)
- `serde_json` 1.0 (no_std + alloc)
- `anova-oven-api` (no_std)

---

## Known Gaps and Gotchas

- **Firebase token expiry:** The server signs into Firebase once at startup.
  Firebase ID tokens expire after 1 hour. Long-running server instances will
  get 401s from Firestore after that point. `firestore.rs` already has a
  `refresh_session()` function ready to use; it just needs to be called (e.g.
  on a background timer or on 401 response). For now, restart the server
  hourly as a workaround.

- **Recipe/history cache invalidation:** Recipes and history are fetched from
  Firestore on the first request and held in memory indefinitely. A server
  restart is required to see new recipes. A `?force_refresh=true` query param
  is the planned solution (Phase 4).

- **WebSocket reconnect backoff:** The current reconnect loop sleeps a flat
  5 s. Phase 4 should replace this with exponential backoff (e.g. 1 s → 2 s →
  4 s → … → 60 s cap).

- **Stop command is fire-and-forget:** `POST /stop` returns 204 as soon as the
  command is queued; it does not wait for a `RESPONSE` frame from the oven.
  The oven does send back `RESPONSE { status: "ok" }` with a matching
  `requestId`, but this is not currently matched or surfaced. For `POST /start`
  (Phase 3), waiting for the response will be more important since a failed
  start should surface an error to the caller. The infrastructure for matching
  responses (a `HashMap<Uuid, oneshot::Sender<String>>` of pending requests)
  does not exist yet.

- **Cooker ID availability:** The cooker ID (needed to address all outbound
  commands) only arrives in `EVENT_APO_WIFI_LIST`, the first message the server
  receives after connecting. Until that message is processed, `POST /stop` and
  any future write endpoints return HTTP 503. In practice this window is very
  short (< 1 s), but clients should handle 503 and retry.

- **Bookmarked recipes are N+1 fetches:** `GET /recipes` first queries
  `users/{uid}/favorite-oven-recipes` to get a list of `recipeRef` document
  references, then issues one individual GET per bookmark to resolve the full
  recipe. There is no batch GET in the Firestore REST API. For users with many
  bookmarks this is slow. The results are cached in memory after the first
  call, so subsequent requests are free.

---

## Reference Material

- WebSocket protocol: [`docs/oven-websocket-api.md`](oven-websocket-api.md)
- Cloud API (Firestore): [`docs/oven-cloud-api.md`](oven-cloud-api.md)
- Legacy WebSocket reference: [`docs/anova-oven-api-reference.md`](anova-oven-api-reference.md)
- Exploration scripts and findings: [`../anova-oven-exploration/`](../anova-oven-exploration/)
- Community protocol docs (Go client): [`../anova-oven-api/`](../anova-oven-api/)
- Official developer docs + PAT management: https://developer.anovaculinary.com/
- Official reference implementation: https://github.com/anova-culinary/developer-project-wifi
