# Anova Precision Oven — WebSocket API

Real-time control of the Anova Precision Oven v1 via the v2 WebSocket protocol.
This is the primary interface for reading oven state and sending cook commands.

> Source: community reverse-engineering ([bogd/anova-oven-api](https://github.com/bogd/anova-oven-api)),
> the official [developer API](https://developer.anovaculinary.com/), and APK analysis.

---

## Connection

**Endpoint:** `wss://devices.anovaculinary.io/`

**Query parameters:**

| Parameter | Value | Notes |
|-----------|-------|-------|
| `token` | PAT or Firebase ID token | See [Authentication](#authentication) |
| `supportedAccessories` | `APO` | Use `APO,APC` if you also have a sous vide cooker |
| `platform` | `android` or `ios` | |

**Required header:**

```
Sec-WebSocket-Protocol: ANOVA_V2
```

**Optional header** (what the Android app sends — may not be required):

```
User-Agent: okhttp/4.7.2
```

**Behavior after connecting:**
- Server sends `EVENT_APO_WIFI_LIST` (contains cooker ID)
- Server sends `EVENT_APO_STATE` every 30s (idle) or every 2s (cooking)
- Server sends `EVENT_USER_STATE` once
- May send `EVENT_APO_WIFI_FIRMWARE_UPDATE` if an OTA is available

---

## Authentication

### PAT (recommended)

Generate in the Anova app: More > Developer > Personal Access Tokens.
Token format: `anova-eyJ0...`

Store in `~/.anova-token`. Use directly as the `token` query parameter.

### Firebase ID Token (alternative)

Exchange a Firebase refresh token for an ID token:

```
POST https://securetoken.googleapis.com/v1/token?key=<FIREBASE_API_KEY>
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token&refresh_token=<REFRESH_TOKEN>
```

Use the `id_token` from the response as the `token` query parameter.

Firebase API keys (both work — same project `anova-app`):
- General app: `AIzaSyB0VNqmJVAeR1fn_NbqqhwSytyMOZ_JO9c`
- Oven app: `AIzaSyCGJwHXUhkNBdPkH3OAkjc9-3xMMjvanfU`

---

## Events (Server → Client)

### EVENT_APO_WIFI_LIST

Sent on connection. Contains the cooker ID needed for all commands.

```json
{
  "command": "EVENT_APO_WIFI_LIST",
  "payload": [
    {
      "cookerId": "01234XXXXXXXXXX",
      "name": "Anova Precision Oven",
      "pairedAt": "2024-01-03T21:58:15.159Z",
      "type": "oven_v1"
    }
  ]
}
```

### EVENT_APO_STATE

Periodic oven state. Every 30s when idle, every 2s during a cook.

```json
{
  "command": "EVENT_APO_STATE",
  "payload": {
    "cookerId": "01234XXXXXXXXXX",
    "state": {
      "nodes": {
        "door": { "closed": true },
        "fan": { "failed": false, "speed": 0 },
        "heatingElements": {
          "bottom": { "failed": false, "on": true, "watts": 0 },
          "rear":   { "failed": false, "on": false, "watts": 0 },
          "top":    { "failed": false, "on": false, "watts": 0 }
        },
        "lamp": { "failed": false, "on": false, "preference": "on" },
        "steamGenerators": {
          "boiler": {
            "celsius": 38.75, "descaleRequired": false, "dosed": false,
            "failed": false, "overheated": false, "watts": 0
          },
          "evaporator": {
            "celsius": 38.75, "failed": false, "overheated": false, "watts": 0
          },
          "mode": "idle",
          "relativeHumidity": { "current": 100 }
        },
        "temperatureBulbs": {
          "dry": {
            "current":  { "celsius": 24.36, "fahrenheit": 75.85 },
            "setpoint": { "celsius": 58.33, "fahrenheit": 137 }
          },
          "dryBottom": {
            "current": { "celsius": 23.73, "fahrenheit": 74.72 },
            "overheated": false
          },
          "dryTop": {
            "current": { "celsius": 24.36, "fahrenheit": 75.85 },
            "overheated": false
          },
          "mode": "dry",
          "wet": {
            "current": { "celsius": 24.36, "fahrenheit": 75.85 },
            "doseFailed": false, "dosed": false
          }
        },
        "temperatureProbe": { "connected": false },
        "timer": { "current": 0, "initial": 0, "mode": "idle" },
        "userInterfaceCircuit": { "communicationFailed": false },
        "vent": { "open": true },
        "waterTank": { "empty": false }
      },
      "state": {
        "mode": "idle",
        "processedCommandIds": ["<uuid>", "..."],
        "temperatureUnit": "F"
      },
      "systemInfo": {
        "firmwareUpdatedTimestamp": "2023-10-18T09:53:56Z",
        "firmwareVersion": "2.1.7",
        "hardwareVersion": "120V Universal",
        "lastConnectedTimestamp": "2024-01-03T21:51:31Z",
        "lastDisconnectedTimestamp": "2024-01-03T21:51:28Z",
        "online": true,
        "powerHertz": 60,
        "powerMains": 120,
        "triacsFailed": false,
        "uiFirmwareVersion": "1.0.22",
        "uiHardwareVersion": "UI_RENASAS"
      },
      "updatedTimestamp": "2024-01-03T22:09:05Z",
      "version": 1
    },
    "type": "oven_v1"
  }
}
```

**Value ranges:**

| Field | Range / Values |
|-------|---------------|
| `fan.speed` | 0–100 |
| `heatingElements.*.watts` | bottom: 0–700, top/rear: 0–1600 |
| `steamGenerators.boiler.watts` | 0–480 |
| `steamGenerators.evaporator.watts` | 0–160 |
| `steamGenerators.relativeHumidity.current` | 0–100 |
| `steamGenerators.mode` | `"idle"`, `"running"` |
| `temperatureBulbs.mode` | `"dry"`, `"wet"` |
| `timer.mode` | `"idle"`, `"running"` |
| `timer.current` | Seconds elapsed |
| `timer.initial` | Seconds total |
| `state.mode` | `"idle"` when not cooking |
| `lamp.preference` | `"on"`, `"off"` |

### EVENT_USER_STATE

```json
{
  "command": "EVENT_USER_STATE",
  "payload": { "is_connected_to_alexa": false }
}
```

### EVENT_APO_WIFI_FIRMWARE_UPDATE

Sent when an OTA firmware update is available.

```json
{
  "command": "EVENT_APO_WIFI_FIRMWARE_UPDATE",
  "payload": {
    "cookerId": "01234XXXXXXXXXX",
    "ota": {
      "available": true,
      "description": "2.1.8 - Prevent cooks when idle or NTC broken ...",
      "required": false,
      "url": "https://storage.googleapis.com/anova-app.appspot.com/oven-firmware/oven-controller-2.1.8.bin",
      "version": "2.1.8"
    },
    "type": "oven_v1",
    "version": "2.1.7"
  }
}
```

### RESPONSE

Sent in reply to any command that includes a `requestId`.

```json
{
  "command": "RESPONSE",
  "requestId": "<matches-your-request>",
  "payload": { "status": "ok" }
}
```

---

## Commands (Client → Server)

All commands that include a `requestId` receive a `RESPONSE` message.
UUIDs can be any v4 UUID.

### CMD_APO_START

Start a cook with one or more stages. A timed cook is typically two stages:
a preheat stage (no timer) followed by a cook stage (with timer).

```json
{
  "command": "CMD_APO_START",
  "payload": {
    "payload": {
      "cookId": "android-<uuid>",
      "stages": [ "...see Stage Format below..." ]
    },
    "type": "CMD_APO_START",
    "id": "<cooker_id>"
  },
  "requestId": "<uuid>"
}
```

### CMD_APO_STOP

Stop the current cook.

```json
{
  "command": "CMD_APO_STOP",
  "payload": {
    "type": "CMD_APO_STOP",
    "id": "<cooker_id>"
  },
  "requestId": "<uuid>"
}
```

### CMD_APO_UPDATE_COOK_STAGES

Replace the full list of stages for the current cook.

```json
{
  "command": "CMD_APO_UPDATE_COOK_STAGES",
  "payload": {
    "payload": {
      "stages": [ "...see Stage Format..." ]
    },
    "type": "CMD_APO_UPDATE_COOK_STAGES",
    "id": "<cooker_id>"
  },
  "requestId": "<uuid>"
}
```

### CMD_APO_UPDATE_COOK_STAGE

Modify a single existing stage (matched by `id`).

```json
{
  "command": "CMD_APO_UPDATE_COOK_STAGE",
  "payload": {
    "payload": { "...full stage object with matching id..." },
    "type": "CMD_APO_UPDATE_COOK_STAGE",
    "id": "<cooker_id>"
  },
  "requestId": "<uuid>"
}
```

### CMD_APO_START_STAGE

Advance to a specific stage (for stages with `userActionRequired: true`).

```json
{
  "command": "CMD_APO_START_STAGE",
  "payload": {
    "payload": {
      "stageId": "android-<uuid>"
    },
    "type": "CMD_APO_START_STAGE",
    "id": "<cooker_id>"
  },
  "requestId": "<uuid>"
}
```

---

## Stage Format

Each cook consists of one or more stages. The stage format is used in
`CMD_APO_START`, `CMD_APO_UPDATE_COOK_STAGES`, and `CMD_APO_UPDATE_COOK_STAGE`,
and is also the format stored in Firestore recipe `steps` arrays.

```json
{
  "stepType": "stage",
  "id": "android-<uuid>",
  "title": "Stage name",
  "description": "",
  "type": "preheat",
  "userActionRequired": false,
  "stageTransitionType": "automatic",
  "temperatureBulbs": {
    "dry": {
      "setpoint": { "fahrenheit": 410, "celsius": 210 }
    },
    "mode": "dry"
  },
  "heatingElements": {
    "bottom": { "on": false },
    "top":    { "on": false },
    "rear":   { "on": true }
  },
  "fan": { "speed": 100 },
  "vent": { "open": false },
  "rackPosition": 3,
  "steamGenerators": {
    "steamPercentage": { "setpoint": 100 },
    "mode": "steam-percentage"
  },
  "timerAdded": true,
  "timer": { "initial": 600 },
  "probeAdded": false,
  "temperatureProbe": {
    "setpoint": { "fahrenheit": 97, "celsius": 36 }
  }
}
```

### Field Reference

| Field | Values | Notes |
|-------|--------|-------|
| `type` | `"preheat"`, `"cook"` | Preheat stages auto-advance when temp is reached |
| `stepType` | `"stage"` | Always `"stage"` |
| `userActionRequired` | `true`/`false` | `false` = auto-advance, `true` = wait for `CMD_APO_START_STAGE` |
| `stageTransitionType` | `"automatic"` | How stages transition |
| `temperatureBulbs.mode` | `"dry"`, `"wet"` | `"wet"` = sous vide mode on |
| `temperatureBulbs.{mode}.setpoint` | `{ fahrenheit, celsius }` | Provide both; unknown which takes precedence if they disagree |
| `heatingElements.{top,bottom,rear}` | `{ "on": bool }` | Each element independent |
| `fan.speed` | 0–100 | |
| `vent.open` | `true`/`false` | |
| `rackPosition` | 1–5 | Tray position from bottom |
| `steamGenerators.mode` | `"steam-percentage"`, `"relative-humidity"` | Choose one mode |
| `steamGenerators.steamPercentage.setpoint` | 0–100 | Only with `mode: "steam-percentage"` |
| `steamGenerators.relativeHumidity.setpoint` | 0–100 | Only with `mode: "relative-humidity"` |
| `timerAdded` | `true`/`false` | Mutually exclusive with `probeAdded` |
| `timer.initial` | Seconds | Only when `timerAdded: true` |
| `probeAdded` | `true`/`false` | Mutually exclusive with `timerAdded` |
| `temperatureProbe.setpoint` | `{ fahrenheit, celsius }` | Only when `probeAdded: true` |

### Converting Firestore Recipes to CMD_APO_START

Firestore recipes (from `oven-recipes` collection) store stages in the `steps` array.
Each step has the same structure as a WebSocket stage. To start a cook from a recipe:

1. Fetch the recipe from Firestore (see [Cloud API docs](oven-cloud-api.md))
2. Extract the `steps` array
3. Use `steps` as the `stages` array in `CMD_APO_START`
4. Generate a fresh `cookId` (e.g., `"cli-<uuid>"`)

The `id` fields on each step can be reused as-is (they were generated by the iOS/Android app).

---

## Service Domains

| URL | Purpose |
|-----|---------|
| `wss://devices.anovaculinary.io/` | v2 device control (current) |
| `wss://app.oven.anovaculinary.io/` | v1 device control (deprecated Aug 2022) |
| `https://developer.anovaculinary.com/` | Official developer docs + PAT management |
