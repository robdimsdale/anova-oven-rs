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
│  └── GET  /history    → simplified cook history JSON │
│                                                      │
│  Internal (hidden from clients):                     │
│  ├── persistent WebSocket → devices.anovaculinary.io │
│  │     (TLS 1.2 via tokio-websockets + native-tls)   │
│  └── Firestore client (reqwest) → Firebase           │
└──────────┬───────────────────────────────────────────┘
           │ plain HTTP, local network (no TLS)
  ┌────────┴────────┐         ┌─────────────────────┐
  │  anova-oven-    │         │  anova-oven-cli      │
  │  pico           │         │                      │
  │                 │         │  Desktop binary.     │
  │  Pico W target. │         │  Prototypes the      │
  │  Connects to    │         │  embedded UI/UX      │
  │  server over    │         │  against the same    │
  │  plain HTTP     │         │  server API.         │
  │  (embassy-net,  │         │  Uses the same       │
  │  no TLS).       │         │  anova-oven-api      │
  │  Uses defmt to  │         │  types.              │
  │  log state.     │         │                      │
  └─────────────────┘         └─────────────────────┘
           │                           │
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
  `wss://devices.anovaculinary.io/` and caches the latest `EVENT_APO_STATE`
  payload in a `tokio::sync::watch` channel. On disconnect it sleeps 5 s
  and reconnects indefinitely.
- A `reqwest` client handles Firebase sign-in and Firestore `runQuery`
  requests. The Firebase session (ID token + refresh token) is cached in
  memory (not on disk).
- Recipe and history data is fetched from Firestore on first request and
  cached in memory for the lifetime of the process.

**Endpoints:**
- `GET /status`  — reads from the `watch` channel, maps to `OvenStatus`.
  Returns HTTP 503 while the WebSocket connection is still establishing.
- `GET /recipes` — queries Firestore `oven-recipes` (user's non-draft
  recipes only), maps to `Vec<Recipe>`.
- `GET /history` — queries `users/{uid}/oven-cooks`, resolves recipe
  titles via individual document GETs, maps to `Vec<HistoryEntry>`.

**Module layout:**
- `src/main.rs`     — entry point, `AppState`, axum setup, WebSocket task,
                      route handlers
- `src/protocol.rs` — Anova WebSocket message parsing (`EVENT_APO_STATE` →
                      `OvenStatus`); absorbed from old `anova-oven-protocol`
- `src/firestore.rs`— Firebase auth (sign-in, token refresh), Firestore
                      `runQuery` + document GET, Firestore Value unwrapping,
                      mapping to `anova-oven-api` types; absorbed from old
                      `anova-oven-firestore`

**Dependencies:** axum 0.7, tokio 1 (full), reqwest 0.12 (rustls-tls),
tokio-websockets 0.13 (native-tls), futures-util, serde_json, `anova-oven-api`.

### `anova-oven-cli`

Desktop binary. Calls the local server. Uses `anova-oven-api` types for
deserialization. Mirrors the embedded client's data flow to validate UI/UX.

**Subcommands:**
- `status`  — `GET /status`, prints human-readable oven state
- `recipes` — `GET /recipes`, lists available recipes
- `history` — `GET /history`, shows recent cooks

**Server address:** `--server <addr>` flag (default `http://localhost:8080`),
also `ANOVA_SERVER` env var. A bare `host:port` without `http://` is accepted
and has the scheme prepended automatically.

**Running:**
```sh
cargo run -p anova-oven-cli -- status
cargo run -p anova-oven-cli -- --server 10.0.1.42:8080 recipes
ANOVA_SERVER=10.0.1.42:8080 cargo run -p anova-oven-cli -- history
```

**Dependencies:** clap 4, reqwest 0.12, tokio 1, serde_json, `anova-oven-api`.

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
panic-probe, embedded-alloc 0.6, reqwless 0.14 (plain HTTP, no TLS),
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

### Phase 3 — Cook Commands

Once the server can receive and send commands over the WebSocket, add write
endpoints:

- `POST /start` with body `{ "recipe_id": "..." }` → `CMD_APO_START`
- `POST /stop` → `CMD_APO_STOP`

Server side:
- The watch-channel pattern already handles state; commands need a separate
  `mpsc` channel from the HTTP handlers into the WebSocket task.
- The WebSocket task sends the command frame and waits for a `RESPONSE` event
  with matching `requestId`.

CLI side: add `start <recipe-id>` and `stop` subcommands.

Pico side: add button input to trigger HTTP POST to `/start` or `/stop`.

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
- `axum` 0.7
- `tokio` 1 (full)
- `reqwest` 0.12 (json, rustls-tls)
- `tokio-websockets` 0.13 (client, native-tls, fastrand, openssl)
- `futures-util` 0.3
- `serde`, `serde_json` 1.0
- `http` 1
- `anova-oven-api`

**`anova-oven-cli`:**
- `clap` 4 (derive, env)
- `reqwest` 0.12 (json, rustls-tls)
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

- **Pico hardware not tested end-to-end:** The pico crate cross-compiles
  cleanly for `thumbv6m-none-eabi` but has not been flashed and tested
  against real hardware with the new server architecture. The reqwless plain
  HTTP path should work but deserves a hardware validation pass.

- **`ANOVA_BIND` not documented in server:** The server respects an
  `ANOVA_BIND` env var (default `0.0.0.0:8080`) to change the listen address.

---

## Reference Material

- WebSocket protocol: [`docs/oven-websocket-api.md`](oven-websocket-api.md)
- Cloud API (Firestore): [`docs/oven-cloud-api.md`](oven-cloud-api.md)
- Legacy WebSocket reference: [`docs/anova-oven-api-reference.md`](anova-oven-api-reference.md)
- Exploration scripts and findings: [`../anova-oven-exploration/`](../anova-oven-exploration/)
- Community protocol docs (Go client): [`../anova-oven-api/`](../anova-oven-api/)
- Official developer docs + PAT management: https://developer.anovaculinary.com/
- Official reference implementation: https://github.com/anova-culinary/developer-project-wifi
