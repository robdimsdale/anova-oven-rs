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

The repo is a Cargo workspace with two member crates, plus a standalone
embedded crate:

```
crates/
  anova-oven-protocol/   # no_std + alloc — shared types + WebSocket event parsing
  anova-oven-cli/        # std — CLI binary (tokio, WebSocket, clap)
  anova-oven-pico/       # no_std, no_main — Pico W firmware (standalone workspace)
```

### What works

- **PAT authentication** — reads `~/.anova-token` (format `anova-eyJ0...`)
- **WebSocket connection** to `wss://devices.anovaculinary.io/`
- **`status`** subcommand — one-shot oven state with human-readable display
- **`watch`** subcommand — continuous state stream
- **`programs`** subcommand — lists local program files from `~/.anova-programs/*.json`
- **Recipe fetching** via Node.js scripts in `../anova-oven-exploration/`
  (outputs JSON to `~/.anova-programs/`)

### What's not implemented

- Command serialization (`Serialize` derives) — the protocol crate is read-only
- Sending cook commands (`CMD_APO_START`, `CMD_APO_STOP`, etc.)
- `start` / `stop` / `update` subcommands
- Native Rust Firestore recipe fetch (currently requires Node.js)
- WebSocket reconnection / backoff
- Graceful SIGINT shutdown
- Pico W: TLS, WebSocket upgrade, and protocol integration (WiFi + TCP works)

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

### Planned: `anova-oven-firestore` (no_std + alloc)

New crate for Firebase auth and Firestore query/response types. Handles
recipe fetching, bookmarks, and cook history.

**Design:** transport-agnostic. The crate constructs HTTP request bodies and parses
response bodies as pure `serde` operations. The caller provides the actual HTTP
transport:
- CLI injects `reqwest`
- Pico W injects `embassy-net` + TLS

This mirrors how `anova-oven-protocol` handles WebSocket messages — it parses
and builds payloads without owning the connection.

**Responsibilities:**
- Firebase auth: construct token exchange request, parse ID token response
- Firestore queries: build structured query JSON (including `DocumentReference`
  field values), parse query response into recipe types
- Recipe, bookmark, and cook history types

### Current: `anova-oven-cli` (std)

CLI binary. Uses `tokio` + `tokio-websockets` for WebSocket, `clap` for args.
Depends on `anova-oven-protocol` with default (std) features.

Has `futures-util` with `sink` feature but does not yet send WebSocket messages.

### Current: `anova-oven-pico` (no_std, standalone workspace)

Pico W embedded firmware. Separate workspace to avoid `critical-section` feature
conflicts. Depends on `anova-oven-protocol` with `default-features = false`.

**Working:** WiFi init, WPA2 connect, DHCP, DNS, raw TCP to port 443.
**Stubbed:** TLS, WebSocket upgrade, protocol message loop.

Since the Pico W has full networking (WiFi + TCP + DNS), it can potentially
talk to both the WebSocket API and Firestore directly. Both need TLS — solving
it once (e.g., with `embedded-tls`) enables both.

### Dependency Graph

```
anova-oven-protocol     (no_std + alloc)
  ├── serde, serde_json
  └── types: OvenState, CookStage, Temperature, etc.

anova-oven-firestore    (no_std + alloc)  ← NEW
  ├── serde, serde_json
  ├── anova-oven-protocol (for shared CookStage/Temperature types)
  └── types: OvenRecipe, FirebaseAuth, FirestoreQuery, etc.

anova-oven-cli          (std)
  ├── anova-oven-protocol (std features)
  ├── anova-oven-firestore (std features)
  ├── tokio, tokio-websockets, reqwest, clap
  └── provides HTTP + WebSocket transports

anova-oven-pico         (no_std, standalone workspace)
  ├── anova-oven-protocol (no_std)
  ├── anova-oven-firestore (no_std)  ← future
  ├── embassy-*, cyw43, cyw43-pio
  └── provides embassy-net transports
```

---

## Implementation Plan

### Phase 1: Native Firestore Recipe Fetch

This is the highest-priority new work — it replaces the Node.js dependency
and enables the Pico W to fetch recipes directly.

1. **Create `anova-oven-firestore` crate** — `no_std + alloc`, same feature
   pattern as `anova-oven-protocol`.

   Types to define:
   - `FirebaseAuthRequest` / `FirebaseAuthResponse` — for token exchange via
     `POST https://securetoken.googleapis.com/v1/token?key=<API_KEY>`
   - `FirestoreQuery` / `FirestoreQueryResponse` — structured query request/response
   - `OvenRecipe` — full recipe document (see [Cloud API](oven-cloud-api.md))
   - `FavoriteOvenRecipe` — bookmark document
   - `OvenCook` — cook history document

   Transport trait (sketch):
   ```rust
   pub trait HttpTransport {
       type Error;
       async fn post(&self, url: &str, body: &[u8], headers: &[(&str, &str)])
           -> Result<Vec<u8>, Self::Error>;
   }
   ```

   The crate provides a `FirestoreClient<T: HttpTransport>` that builds the
   correct Firestore REST API request bodies (including `DocumentReference`
   values) and parses responses.

   > **Critical:** The Firestore security rules require the exact query shape:
   > `where("userProfileRef", "==", <DocumentReference>)` combined with
   > `where("draft", "==", false)`. See [Cloud API — Security Rules](oven-cloud-api.md#firestore-security-rules).

2. **Add `CookStage` type to `anova-oven-protocol`** — shared between
   WebSocket commands and Firestore recipe steps. Add `Serialize` derives
   to the types that need to be sent (stages, commands).

3. **Add `fetch-recipes` subcommand to CLI** — authenticates via email/password
   or refresh token, queries Firestore, writes JSON to `~/.anova-programs/`.
   Provide a `reqwest`-based `HttpTransport` implementation.

4. **Bookmarks and cook history** — extend the CLI to also fetch:
   - `users/{uid}/favorite-oven-recipes` (ordered by `addedTimestamp`)
   - `users/{uid}/oven-cooks` (ordered by `endedTimestamp`)

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

13. **Fix Pico W compile errors** — embassy/cyw43 API has changed. Known issues:
    `PioSpi::new` signature, `cyw43::new` arity, `control.join` API,
    `DhcpConfig` fields, `embassy_net::new` seed type, firmware alignment.

14. **TLS** — add `embedded-tls` or similar. This unlocks both WebSocket and
    Firestore on the Pico.

15. **WebSocket integration** — upgrade TCP to WSS, run the protocol message
    loop using `anova-oven-protocol::parse_message()`.

16. **Firestore on Pico** — provide an `embassy-net` `HttpTransport` impl.
    The Pico could fetch recipes directly over WiFi, or use pre-fetched JSON
    from flash/SD as a fallback.

---

## Key Dependencies

**Protocol crate (current):**
- `serde` 1.0 (default-features=false, features: derive, alloc)
- `serde_json` 1.0 (default-features=false, features: alloc)

**Firestore crate (planned):**
- `serde`, `serde_json` (same as above)
- `anova-oven-protocol` (path dep)

**CLI (current):**
- `clap` 4 (derive, env)
- `tokio` 1 (full)
- `tokio-websockets` 0.13 (client, native-tls, fastrand, openssl)
- `futures-util` 0.3 (sink)
- `serde`, `serde_json` 1.0

**CLI (planned additions):**
- `reqwest` — HTTP for Firebase auth + Firestore REST
- `uuid` — generating `requestId` and `cookId` values

**Pico W (current):**
- `embassy-executor`, `embassy-rp`, `embassy-net`, `embassy-time`
- `cyw43`, `cyw43-pio`
- `cortex-m`, `cortex-m-rt`, `defmt`, `panic-probe`

**Pico W (planned additions):**
- `embedded-tls` — TLS over TCP (unlocks both WSS and HTTPS)

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

The format is the raw Firestore `oven-recipes` document. Each file's `steps`
array contains both `"stage"` entries (oven stages) and `"direction"` entries
(text instructions). Filter to `stepType == "stage"` before sending to
`CMD_APO_START`.

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
