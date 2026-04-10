# Architecture and Implementation Plan

## Project Goal

Build a Rust CLI and embedded firmware (Raspberry Pi Pico W) to control an
Anova Precision Oven v1 as a replacement for the Anova mobile app.

Target capabilities:
- Read oven state (temperature, heating elements, steam, timer, etc.)
- List and run saved cook programs (multi-stage APO cooks)
- Send cook commands (`CMD_APO_START`, `CMD_APO_STOP`, etc.)
- Fetch the user's cloud-stored recipes from Firestore

---

## Current State

The repo is a Cargo workspace with three member crates, plus a standalone
embedded crate:

```
crates/
  anova-oven-protocol/   # no_std + alloc — shared types + WebSocket event parsing
  anova-oven-firestore/  # no_std + alloc — Firebase auth + Firestore queries + recipe types
  anova-oven-cli/        # std — CLI binary (tokio, WebSocket, reqwest, clap)
  anova-oven-pico/       # no_std, no_main — Pico W firmware (standalone workspace)
```

### What works

- **PAT authentication** — reads `~/.anova-token` (format `anova-eyJ0...`)
- **WebSocket connection** to `wss://devices.anovaculinary.io/`
- **`status`** subcommand — one-shot oven state with human-readable display
- **`watch`** subcommand — continuous state stream
- **`programs`** subcommand — lists cached recipes from `~/.anova-programs/*.json`
- **`fetch-recipes`** subcommand — authenticates via Firebase email/password
  (or cached refresh token), queries Firestore for the user's recipes, and
  writes JSON to `~/.anova-programs/`. Optional `--bookmarks` flag fetches
  bookmarked community recipes too. Refresh token is cached at
  `~/.anova-firebase-refresh-token` for future runs.
- **Pico W library target** (`src/lib.rs`) — demonstrates the same Firestore
  code paths (auth + recipe fetch) behind an `HttpClient` trait stub. Compiles
  for `thumbv6m-none-eabi`.

### What's not implemented

- Command serialization (`Serialize` derives) — the protocol crate is read-only
- Sending cook commands (`CMD_APO_START`, `CMD_APO_STOP`, etc.)
- `start` / `stop` / `update` subcommands
- WebSocket reconnection / backoff
- Graceful SIGINT shutdown
- Pico W binary: compiles with full TLS + WebSocket + Firestore HTTP stack,
  but not yet tested on hardware. TLS certificate verification is disabled.

---

## Crate Architecture

### Current: `anova-oven-protocol` (no_std + alloc)

Shared types and WebSocket event deserialization. Conditionally `no_std` via
`#![cfg_attr(not(feature = "std"), no_std)]` with feature `default = ["std"]`.

**Types defined:** `Event`, `ApoStatePayload`, `OvenState`, `Nodes`, `StateInfo`,
`SystemInfo`, `Door`, `Fan`, `HeatingElements`, `HeatingElement`,
`SteamGenerators`, `RelativeHumidity`, `TemperatureBulbs`, `DryBulb`, `WetBulb`,
`Temperature`, `TemperatureProbe`, `Timer`, `Vent`, `WaterTank`.

**Public API:** `parse_message(data: &[u8]) -> Result<Event, ...>` — dispatches
on the WebSocket `command` field.

**Gaps:**
- No `Serialize` — cannot build commands to send
- No `CookStage` type — stage format is only handled as raw `serde_json::Value`
  in the CLI
- No recipe/program types

### Current: `anova-oven-firestore` (no_std + alloc)

Transport-agnostic Firebase Auth + Firestore client for the Anova Oven app's
backend. Same `no_std` / `std` feature pattern as `anova-oven-protocol`.

**Modules:**
- `auth` — Firebase Identity Toolkit sign-in (`signInWithPassword`) and Secure
  Token Service refresh. Builds request bodies and parses responses as pure
  serde operations. The caller provides the actual HTTP transport.
- `firestore` — Firestore REST `runQuery` structured query builder (including
  `DocumentReference` field values, composite filters, ordering, and limits),
  plus document and response types. `Document::to_json()` unwraps the verbose
  Firestore Value type tags into plain JSON.
- `value` — `FirestoreValue` type representing the Firestore REST wire format
  (`stringValue`, `booleanValue`, `referenceValue`, `mapValue`, etc.) with
  conversion to `serde_json::Value`.
- `queries` — Pre-built query constructors matching the exact shapes required
  by Anova's Firestore security rules: `user_recipes`, `user_draft_recipes`,
  `published_recipes`, `favorite_recipes`, `oven_cooks`.
- `recipe` — High-level document types: `OvenRecipe`, `FavoriteOvenRecipe`,
  `OvenCook`, `Ingredient`, `Step`. Each has `from_document(&Document)` for
  ergonomic deserialization from Firestore responses.

**Constants:** `ANOVA_PROJECT_ID`, `ANOVA_OVEN_API_KEY`, `ANOVA_GENERAL_API_KEY`.

**Tests:** `tests/parse.rs` covers response parsing, query shape validation,
single-document parsing, and refresh token URL encoding.

### Current: `anova-oven-cli` (std)

CLI binary. Uses `tokio` + `tokio-websockets` for WebSocket, `reqwest` for
Firestore REST + Firebase auth, `clap` for args.

**Subcommands:**
- `status` — one-shot oven state
- `watch` — continuous state stream
- `programs` — lists cached recipes from `~/.anova-programs/`
- `fetch-recipes` — authenticates with Firebase, fetches user recipes from
  Firestore, writes JSON to `~/.anova-programs/`. Supports `--email`,
  `--password`, `--bookmarks` flags plus `ANOVA_EMAIL` / `ANOVA_PASSWORD` env
  vars. Caches the refresh token at `~/.anova-firebase-refresh-token`.

### Current: `anova-oven-pico` (no_std, standalone workspace)

Pico W embedded firmware. Separate workspace to avoid `critical-section` feature
conflicts. Depends on `anova-oven-protocol` and `anova-oven-firestore` with
`default-features = false`.

**Library target** (`src/lib.rs`): Demonstrates the Firestore integration code
path for embedded — defines an `HttpClient` trait with `post_json` / `post_form`
methods for the caller to implement over `embassy-net` + TLS. Provides
`sign_in()` and `fetch_user_recipes()` functions that compose the firestore
crate's query builders into a complete recipe-fetch flow. Compiles clean for
`thumbv6m-none-eabi`.

**Binary target** (`src/main.rs`): WiFi init, WPA2 connect, DHCP, DNS, then
runs two sequential phases: (1) Firestore recipe fetch via `reqwless` HTTPS,
(2) Anova WebSocket connection via `embedded-tls` + `websocketz`. Compiles
clean for `thumbv6m-none-eabi`.

**Modules:**
- `src/lib.rs` — `HttpClient` trait + `sign_in()` / `fetch_user_recipes()`
  (transport-agnostic, consumed by `http.rs`)
- `src/http.rs` — `PicoHttpClient` implementing `HttpClient` via `reqwless`
  + `embedded-tls` over embassy-net's `TcpClient` / `DnsSocket`
- `src/ws.rs` — WebSocket-over-TLS connection to the Anova API using
  `embedded-tls` directly + `websocketz`, with protocol message loop via
  `anova_oven_protocol::parse_message()`
- `src/rng.rs` — SplitMix64 PRNG implementing `rand_core` 0.6 traits
  (for `embedded-tls`); `rand::rngs::SmallRng` (rand 0.10) used for
  `websocketz`

### Dependency Graph

```
anova-oven-protocol     (no_std + alloc)
  ├── serde, serde_json
  └── types: OvenState, CookStage, Temperature, etc.

anova-oven-firestore    (no_std + alloc)
  ├── serde, serde_json
  └── types: OvenRecipe, SignInRequest/Response, RunQueryRequest, etc.

anova-oven-cli          (std)
  ├── anova-oven-protocol (std features)
  ├── anova-oven-firestore (std features)
  ├── tokio, tokio-websockets, reqwest, clap, rpassword
  └── provides HTTP + WebSocket transports

anova-oven-pico         (no_std, standalone workspace)
  ├── anova-oven-protocol (no_std)
  ├── anova-oven-firestore (no_std)
  ├── serde_json (no_std, alloc)
  ├── embassy-*, cyw43, cyw43-pio
  ├── embedded-tls 0.18 (TLS 1.3)
  ├── reqwless 0.14 (HTTPS client, uses embedded-tls internally)
  ├── websocketz 0.2 (WebSocket over embedded-io-async)
  └── defines HttpClient trait + PicoHttpClient impl
```

---

## Implementation Plan

### Phase 1: Native Firestore Recipe Fetch — COMPLETE

The `anova-oven-firestore` crate and `fetch-recipes` CLI subcommand are
implemented and working. The Node.js scripts in `../anova-oven-exploration/`
are no longer needed for recipe fetching.

**What was built:**
- `anova-oven-firestore` crate — Firebase auth (sign-in + refresh token) and
  Firestore structured query builder/parser. Transport-agnostic (pure serde).
  Compiles for both std and `thumbv6m-none-eabi` (no_std + alloc).
- `fetch-recipes` CLI subcommand — authenticates via email/password or cached
  refresh token, runs the exact `runQuery` shape Firestore security rules
  require, writes JSON to `~/.anova-programs/`. Optional `--bookmarks` flag.
- Pico W library target — same code paths behind an `HttpClient` trait,
  compiles for the embedded target.
- Integration tests for response parsing and query shape validation.

**What was NOT done in Phase 1:**
- `CookStage` type in protocol crate — stages remain `serde_json::Value`
- Pico W binary compile fix (done in Phase 4, item 13)
- Cook history fetch not wired into CLI (queries module is ready)

### Phase 2: Cook Commands

5. **Add `Serialize` to protocol types** — enable building `CMD_APO_START` etc.

6. **`CMD_APO_START` from a local recipe** — read a JSON from
   `~/.anova-programs/`, filter `steps` to `stepType == "stage"` only (skip
   `"direction"` entries), send as `stages`. See
   [WebSocket API — Converting Firestore Recipes](oven-websocket-api.md#converting-firestore-recipes-to-cmd_apo_start).

   CLI: `anova-oven start <recipe-name-or-file>`

7. **`CMD_APO_STOP`** — trivial one-shot command. CLI: `anova-oven stop`

8. **`CMD_APO_UPDATE_COOK_STAGE`** — mid-cook tweaks (temperature, timer).

9. **`CMD_APO_START_STAGE`** — advance to next stage when
   `userActionRequired: true`.

### Phase 3: Usability

10. **Reconnection and backoff** — WebSocket reconnect with exponential backoff.
    Graceful shutdown on SIGINT.

11. **TUI mode** — live-updating terminal display with keyboard controls for
    starting stages, updating temperature, and stopping.

12. **Human-friendly recipe format** — optionally define TOML/YAML format for
    hand-authored cook programs. The raw Firestore JSON already works.

### Phase 4: Embedded (Raspberry Pi Pico W)

13. **Fix Pico W compile errors** — COMPLETE. Migrated to embassy 0.10 / cyw43
    0.7 APIs: `PioSpi::new` signature (with `DEFAULT_CLOCK_DIVIDER`, separate DMA
    channel), `cyw43::new` 5-arg form (state, pwr, spi, FW, NVRAM) with separate
    `control.init(CLM)`, firmware `Aligned` wrapper, `JoinOptions::new()`,
    `Config::dhcpv4(Default::default())`, `embassy_net::new` with `u64` seed,
    `embedded-alloc` global allocator for `extern crate alloc`, and `memory.x`
    linker script for RP2040. Both lib and binary targets compile clean.

14. **TLS** — COMPLETE. Added `embedded-tls` 0.18 (TLS 1.3, no_std). Used
    directly by `ws.rs` for WebSocket, and internally by `reqwless` for HTTPS.
    TLS certificate verification is skipped (`TlsVerify::None` / `UnsecureProvider`)
    — acceptable for a personal IoT device, but a production build should add
    certificate pinning. Shared 16,640-byte TLS read/write record buffers
    (static) between HTTP and WebSocket phases to stay within 264KB RAM.
    Required workaround: `der` crate needs explicit `heapless` feature
    (`embedded-tls` 0.18 doesn't enable it for `rustpki`).

15. **WebSocket integration** — COMPLETE. Added `websocketz` 0.2 (zero-copy,
    `embedded-io-async` 0.7). `ws.rs` performs: DNS resolve → TCP connect →
    TLS handshake → WebSocket upgrade with `Sec-WebSocket-Protocol: ANOVA_V2`
    header and token query parameter → message loop calling
    `anova_oven_protocol::parse_message()` on each text frame. Uses
    `rand::rngs::SmallRng` (rand 0.10) for WebSocket masking. Two `rand_core`
    versions coexist: 0.6 (embedded-tls) and 0.10 (websocketz via rand 0.10).

16. **Firestore on Pico** — COMPLETE. Added `reqwless` 0.14 (HTTPS client with
    built-in `embedded-tls` integration). `http.rs` implements the `HttpClient`
    trait from `lib.rs` via `PicoHttpClient` wrapping reqwless's `HttpClient`
    with embassy-net's `TcpClient` + `DnsSocket`. `run_firestore_flow()`
    calls `sign_in()` → `fetch_user_recipes()` from the library target. The
    full flow (Firebase auth + Firestore structured query) compiles for
    `thumbv6m-none-eabi`. Not yet tested on hardware.

---

## Key Dependencies

**Protocol crate:**
- `serde` 1.0 (default-features=false, features: derive, alloc)
- `serde_json` 1.0 (default-features=false, features: alloc)

**Firestore crate:**
- `serde`, `serde_json` (same as above)

**CLI:**
- `clap` 4 (derive, env)
- `tokio` 1 (full)
- `tokio-websockets` 0.13 (client, native-tls, fastrand, openssl)
- `futures-util` 0.3 (sink)
- `reqwest` 0.12 (json, rustls-tls)
- `rpassword` 7
- `serde`, `serde_json` 1.0

**Pico W:**
- `embassy-executor` 0.10, `embassy-rp` 0.10, `embassy-net` 0.9, `embassy-time` 0.5
- `cyw43` 0.7, `cyw43-pio` 0.10
- `cortex-m`, `cortex-m-rt`, `defmt`, `panic-probe`
- `embedded-alloc` 0.6 (global allocator for `extern crate alloc`)
- `embedded-tls` 0.18 (TLS 1.3, no_std, defmt)
- `reqwless` 0.14 (HTTPS client with built-in embedded-tls integration)
- `websocketz` 0.2 (WebSocket, zero-copy, embedded-io-async 0.7)
- `rand` 0.10 (SmallRng for websocketz), `rand_core` 0.6 (for embedded-tls)
- `serde_json` 1.0 (no_std, alloc)

---

## Local Recipe Storage

Recipes are cached as JSON files in `~/.anova-programs/`:

```
~/.anova-programs/
  gf-sourdough-new.json
  freezer-reheat.json
  slow-cook-meat.json
  ...
  _all-recipes.json       # combined file
  _bookmarks.json         # bookmarked community recipes
```

The format matches the Firestore `oven-recipes` document, unwrapped from
Firestore's verbose Value type tags into plain JSON (e.g. timestamps become
ISO 8601 strings, document references become their full path strings, integers
are plain numbers). Each file's `steps` array contains both `"stage"` entries
(oven stages) and `"direction"` entries (text instructions). Filter to
`stepType == "stage"` before sending to `CMD_APO_START`.

---

## Reference Material

- WebSocket protocol: [`docs/oven-websocket-api.md`](oven-websocket-api.md)
- Cloud API (Firestore): [`docs/oven-cloud-api.md`](oven-cloud-api.md)
- Legacy WebSocket reference: [`docs/anova-oven-api-reference.md`](anova-oven-api-reference.md)
- Exploration scripts and findings: [`../anova-oven-exploration/`](../anova-oven-exploration/)
- Community protocol docs (Go client): [`../anova-oven-api/`](../anova-oven-api/)
- Official developer docs + PAT management: https://developer.anovaculinary.com/
- Official reference implementation: https://github.com/anova-culinary/developer-project-wifi
- WebSocket simulator for testing: https://github.com/apassuello/chef-gpt
