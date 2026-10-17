# Emulating the oven's cloud

Everything needed to stand in for `wss://iot-prod.anovaculinary.io`, the
server the oven itself connects to, so the oven can be run with no Anova cloud
at all. This is the **device-side** protocol. The app-side protocol
(`devices.anovaculinary.io`) is in [oven-websocket-api.md](oven-websocket-api.md).

Unless marked otherwise, everything here was observed on 2026-10-14 against an
Anova Precision Oven v1 (`hardwareVersion 120V1`, firmware **2.1.16**). It
comes from three sources:
- the oven's own logs ([oven-uart.md](oven-uart.md));
- what the real cloud sent before the redirect, as recorded in that log;
- commands sent from [tools/oven-cloud-probe.py](../tools/oven-cloud-probe.py).

## Redirecting the oven

The oven looks up `iot-prod.anovaculinary.io` through whatever DNS server
DHCP gives it. Point that name at the
emulator in the LAN's DNS (usually a local DNS record on the router).

- **An A record** works (tested).
- **A CNAME** should work if the router's own DNS can resolve the target name,
  so that it returns the CNAME *and* the target's address in one answer. This
  is **untested**: it follows from how lwIP's resolver works, which does not
  chase a CNAME with a second query. The router
  may learn DHCP clients as `<host>` and `<host>.localdomain`, so a CNAME to
  `<host>.localdomain` should work. A CNAME to `<host>.local` will **not**:
  `.local` is mDNS, which the router's DNS
  doesn't answer. Check before relying on it:
  ```sh
  dig iot-prod.anovaculinary.io @<router-ip>   # the ANSWER needs both the CNAME and an A record
  ```
  Moving the CNAME switches between emulators. To go back to Anova, remove
  the record, or point it at the public record's own target, which is itself a
  CNAME to `d-yitox3hesi.execute-api.us-west-2.amazonaws.com`. The second
  only works if the router returns that name's A records in the same answer,
  which dnsmasq may not do for an external target, and the target is Anova's
  and could change.
- **The switch takes effect when the oven reconnects.** Power-cycle the oven
  after changing the record.

## Connection

| | |
|---|---|
| Port | 443 |
| TLS | 1.2 (`ECDHE-RSA-AES256-GCM-SHA384` negotiated). **No SNI.** |
| Client certificate | None requested. The listener never asked for one; the oven has a secure element and might present one if asked. |
| WebSocket | RFC 6455, `GET /` |

The upgrade request in full (headers in the order sent):

```
GET / HTTP/1.1
Connection: Upgrade
Host: iot-prod.anovaculinary.io:443
User-Agent: ESP32 Websocket Client
Upgrade: websocket
Sec-WebSocket-Version: 13
Sec-WebSocket-Key: <base64>
Authorization: <JWT>
```

There is no subprotocol and no query string. Answer with a plain `101` and
`Sec-WebSocket-Accept`.

The JWT is ES256, signed by the board's ATECC secure element, and valid for
24 hours (`exp - iat = 86400`). Its claims:

```json
{"iat": 1792017645, "exp": 1792104045, "deviceId": "<18 hex chars>",
 "deviceClass": "oven_v1", "deviceType": "esp32", "softwareVersion": "2.1.16",
 "hardwareVersion": "120V1", "releaseTrack": "production"}
```

An emulator can ignore it, or use `deviceId` to tell ovens apart. Verifying
the signature would need the device's public key, which we don't have.

**Framing:**
- **The oven pings every 10s.** Answer with a pong.
- **The oven sends text frames, sometimes split.** The first `state` after
  connecting arrived as a text frame with `fin=0` plus a continuation, so
  fragments must be joined.
- **Commands to the oven** go in single unmasked text frames.

**Reconnecting:** if the server goes away, the oven retries about every 0.5s
(`esp-tls: ... Connection reset by peer`) and was back within 2s of a restart.
The only disconnect tested was while the oven was idle, so whether a cook in
progress carries on through one is **untested**.

## Oven → server

Every message is `{"action": "<name>", "data": {...}}`. Note the spaces after
the colons in `"action": "pairing"` and the others built from string
templates; `state` and `diagnostic` are compact.

### `pairing`

```json
{"action": "pairing","data": {"userId": "<Firebase uid of the Anova account>"}}
```

Sent right after the first connection after boot. A reconnect sends `state`
straight away instead. Needs no reply.

### `state`

The oven's complete state, never a diff. It appears to be the `payload.state` of the
app-side `EVENT_APO_STATE` ([oven-websocket-api.md](oven-websocket-api.md#event_apo_state)),
plus a `cook` block while a cook is active. `data` is `{version, updatedTimestamp,
systemInfo, state, nodes, cook}`; timestamps are UTC.

- **`state.mode`** is `idle` or `cook` in everything seen. The oven also
  has a descale mode (`CMD_APO_START_DESCALE`, untested), so an emulator
  should expect other values. `state.temperatureUnit` is `F` or `C`.
  Temperatures are always reported in both units regardless.
- **`state.processedCommandIds`** holds the last 10 command IDs the oven has
  handled, newest first. **Every** command lands here, `CMD_APO_HEALTHCHECK`
  included, so at the 10s healthcheck cadence an ID is gone after about 100s
  unless something else is sent. A server that wants confirmation must read it
  from the next `state` after sending.
- **`cook`** exists only during a cook and disappears on stop:
  ```json
  {"cookId": "<uuid>", "stages": [ ... ], "activeStageId": "<uuid>",
   "activeStageIndex": 0, "stageTransitionPendingUserAction": false,
   "activeStageSecondsElapsed": 31, "secondsElapsed": 31}
  ```
  `stages` is the oven's own copy of the recipe, cut down to `title`, `id`,
  `type`, `userActionRequired`, `temperatureBulbs`, `timer`, `heatingElements`,
  `fan`, `vent`, plus `temperatureProbe` and `steamGenerators` where set.
  `description`, `rackPosition`, `stageTransitionType`, `timerAdded` and
  `probeAdded` are dropped. `secondsElapsed` is for the whole cook,
  `activeStageSecondsElapsed` for the current stage.
- **Steam** (seen only in the first minute of a cook). `steamGenerators.mode` is
  `idle`, `relative-humidity` (with `relativeHumidity.setpoint`, a percentage)
  or `steam-percentage` (with `steamPercentage.setpoint`). A stage with
  `temperatureBulbs.mode: "wet"` carries a `wet.setpoint` in place of
  `dry.setpoint`, and the state then has no `dry.setpoint`. While dosing,
  `temperatureBulbs.wet.dosed` is `true`.

**When it is sent:**
- On connect (after `pairing` on the first connection after boot).
- Every **10s while cooking** and, by default, only about **every 10 minutes
  while idle** (9m45s between a stop and the next report). We watched the
  oven directly with no cloud involved, so this is the oven's own rate, not
  cloud throttling. The app-side docs' 10 minutes corroborates it.
- Straight after a command that changes the cook (start, stop, stage change,
  setpoint, timer), the temperature unit or the lamp preference: within about
  1.5s. `SET_FAN`, `SET_VENT`, `SET_PROBE` and `SET_HEATING_ELEMENTS` showed up
  5 to 10s later, in the next periodic message. `SET_LAMP` took 1.3s once and
  10s another time.
- **`CMD_APO_SET_REPORT_STATE_RATE`** `{"idle": 30, "cooking": 10}` is acked.
  Our first `{"idle": 30, "cooking": 2}` (16:04:57) started the 30s idle
  cadence. `cooking: 2` was sent while idle, so its effect is untested here. The
  real cloud sent `{"idle":30,"cooking":2}` at 15:39, yet the first probe cook
  at 15:50 reported every 10s. Either the rate doesn't survive a reconnect or
  `cooking: 2` isn't honoured. **An emulator should send the rate on every
  connect.**

The first `state` after connecting may arrive fragmented (see Framing above).

### `healthcheck`

```json
{"action": "healthcheck","data": {}}
```

The reply to `CMD_APO_HEALTHCHECK`, under 150ms after it. The real cloud
sent one every 10s and the probe copied that. Whether the oven drops the
session without them wasn't tested.

### `timezone`

```json
{"action": "timezone","data": {}}
```

A request for the time zone. The first one came in the same second as
`TIMEZONE: deleted timezone file` (15:50:40, 16s after our first
`CMD_APO_START`). That is one coincidence, so cause and effect is unproven,
and why the file was deleted is unknown. Unanswered, it repeats with backoff: gaps of 27s, 31s, 54s,
84s, 141s and 323s. It never blocked anything.

The answer is `CMD_APO_SET_TIME_ZONE` with
`{"time_zone": {"code": "PST", "gmt_offset": -28800, "id": "America/Los_Angeles"}}`
(the payload is identical to the real cloud's apart from the ID format).

- **It is applied, not just acked.** The oven logs its clock when the zone
  changes: our command at 16:04:25 changed its offset from UTC−6 to UTC−8 (local
  readings of 17:01 and 15:04, logged 3 minutes apart). UTC−6 was what it ran
  on with no timezone (17:01 local at 23:01 UTC).
- **`PST`/-28800 sets the clock an hour wrong in daylight time.** It is UTC−8
  and the date was in UTC−7. After the real cloud's answer and after ours the
  oven read 13:34 and 15:04, an hour behind. Untested: whether
  `gmt_offset: -25200` fixes it, or whether `id` drives daylight-saving rules.
- **It keeps asking after an answer:** a request came at 16:17:05, 12½
  minutes after ours. The console doesn't log outgoing messages, so the only
  known real-cloud answers are the `SET_TIME_ZONE` times 14:34, 14:50 and
  15:20; that they replied to requests is an inference.

### `diagnostic`

Reply to `CMD_APO_REQUEST_DIAGNOSTIC`, compact JSON, about 100ms later:

```json
{"action":"diagnostic","data":{"ovenId":"<18 hex chars>","systemMicroSeconds":1596341476,"resetReason":1}}
```

The request is `{"command": "reset-reason"}`; `resetReason` is presumably
ESP-IDF's `esp_reset_reason_t` (1 = power-on).

## Server → oven

Each command is `{"id": "<uuid>", "type": "CMD_APO_…", "payload": {…}}`, sent
as one text frame. The `id` should be unique: it is what comes back in
`processedCommandIds`. These were all sent through the probe's control port.
"Acked" means the ID came back in `processedCommandIds`.

| Command | Payload | Observed |
|---|---|---|
| `CMD_APO_HEALTHCHECK` | none | Answered with `healthcheck`. |
| `CMD_APO_START` | `{cookId, stages: [...]}` | Starts stage 0 at once. Stage format as in [oven-websocket-api.md](oven-websocket-api.md#stage-format). **Replaces a cook already running**, with no stop first. |
| `CMD_APO_STOP` | none | `mode: idle` and `cook` gone within a second. Works in any cook state. |
| `CMD_APO_START_NEXT_STAGE` | none | Starts the next stage within 1.2s, even with time left on the current one. On the last stage it only sets `stageTransitionPendingUserAction: true` and the timer carries on. |
| `CMD_APO_START_STAGE` | `{"stageId": "<uuid>"}` | Jumps to that stage; this is how a stage waiting on the user is released. |
| `CMD_APO_UPDATE_COOK_STAGES` | `{stages: [...]}` | Replaces the stored list mid-cook. The active stage (matched by `id`) carried on, and stages could be added after it. |
| `CMD_APO_SET_TIMER` | `{"initial": 60}` | Restarts the running countdown from 60s and updates the stage's stored `timer.initial`. |
| `CMD_APO_SET_TEMPERATURE_BULBS` | `{"mode": "dry", "dry": {"setpoint": {...}}}` | Changes the setpoint, in the stored stage too. |
| `CMD_APO_SET_PROBE` | `{"setpoint": {"celsius": 55, "fahrenheit": 131}}` | Stored as the stage's `temperatureProbe`. No probe was attached. The target is **dropped** when the preheat moves to the cook stage (gone from `nodes.temperatureProbe`), and `UPDATE_COOK_STAGES` removed it from the stored preheat because the replacement list lacked it. |
| `CMD_APO_SET_HEATING_ELEMENTS` | `{"top": {"on": true}, "rear": {...}, "bottom": {...}}` | Stored in the active stage. |
| `CMD_APO_SET_VENT` | `{"open": true}` | `vent.open` followed within 10s and went back to the stage's value when told to close. |
| `CMD_APO_SET_FAN` | `{"speed": 50}` | Acked, but refused: the oven logged `Fan speed of 50% is not valid (rear element is on)...` and kept 100%. A firmware rule (the rear element needs full fan), not an ignored command. A lower speed with only the top or bottom element on is untested. |
| `CMD_APO_SET_LAMP` | `{"on": true}` | Worked mid-cook in both directions. Idle: no effect (`lamp.on` stayed `false`). |
| `CMD_APO_SET_LAMP_PREFERENCE` | `{"on": true}` | See below. |
| `CMD_APO_SET_TEMPERATURE_UNIT` | `{"temperatureUnit": "C"}` | `state.temperatureUnit` flipped within a second. |
| `CMD_APO_SET_REPORT_STATE_RATE` | `{"idle": 30, "cooking": 10}` | See `state` above. |
| `CMD_APO_SET_TIME_ZONE` | see `timezone` | Acked and applied to the oven's clock. |
| `CMD_APO_REQUEST_DIAGNOSTIC` | `{"command": "reset-reason"}` | Answered with `diagnostic`. |

Not tried: `CMD_APO_SET_STEAM_GENERATORS`, `_START_DESCALE`, `_ABORT_DESCALE`,
`_OTA`, `_UPDATE_COOK_STAGE` (singular), `_SET_REPORT_STATE_RATE_DEFAULT`.
Also untested: what the oven reports when the door opens (`door.closed`) or
the water tank runs empty (`waterTank.empty`).

**Lamp preference takes `{"on": bool}`, not `{"preference": "on"|"off"}`.**
`{"preference": "off"}` did turn it off, but `{"preference": "on"}` twice did
nothing, and `{"on": true}` turned it on. The oven appears to read only
`on`, treating a missing one as `false`.

**Malformed input:**
- An unknown `type` is acked and does nothing.
- A message with **no `type`** is ignored and not acked.
- **`expiresAt` is not enforced.** A `CMD_APO_SET_LAMP` with an `expiresAt` 3h
  in the past was applied.

## Running a cook

What the oven does with a recipe, as seen in the state messages. A server
only has to deliver commands and, at the end, `CMD_APO_STOP`.

- **A `preheat` stage has no timer of its own.** The timer shows the *next*
  cook stage's `initial` in mode `idle`. When the preheat ends, the next stage
  begins and its timer starts running. A `cook` stage with no preceding
  preheat starts its timer at once, not at temperature.
- **A preheat ends when the cavity reaches its setpoint.** A preheat to 60°C
  was still going at 57.95°C after 2.5 minutes. Lowering its setpoint to 58°C
  ended it within 2s. The oven logged `Preheat target dry-temperature reached (57.93 >= 58.00)`,
  so it ended about 0.1°C short, from one sample; the tolerance is unknown.
- **When a stage's timer ends**, one of two things happens:
  - If the next stage has `userActionRequired: false`, it starts on its own,
    to the second (stage 2 → 3 after exactly 60s).
  - If the next stage has `userActionRequired: true`, or there is no next
    stage, the oven sets `stageTransitionPendingUserAction: true`, sets the
    timer to `idle` with `initial: 0`, keeps holding the setpoint and keeps
    counting `activeStageSecondsElapsed`. It waits for `CMD_APO_START_STAGE` /
    `CMD_APO_START_NEXT_STAGE` or `CMD_APO_STOP`.

  So a stage's `userActionRequired` decides whether the oven waits **before
  entering it**, not when it ends.
- **A preheat and the cook after it are a pair.** The first `SET_*` command
  during a preheat copied the preheat's setpoint onto the following cook
  stage (62°C became 60°C), and `SET_VENT` was applied to both. `SET_PROBE`
  went to the preheat stage only. The console logged
  `preheat/cook: update settings` on every `SET_*` during the preheat. (The
  firmware's error variant, `Failed to update preheat/cook stage pair`, never
  appeared.) A server that wants the pair to
  keep its own setpoints shouldn't send `SET_*` during a preheat; send
  `UPDATE_COOK_STAGES` instead.
- **Edits made with `SET_*` or `UPDATE_COOK_STAGES` show up in `cook.stages`,**
  which is how a server can read back what the oven believes the recipe is.
  `UPDATE_COOK_STAGES` restored the original setpoints that earlier `SET_*`
  commands had changed.
- **The lamp comes on by itself** at the start of a cook if
  `lamp.preference` is `on`.
- **At the end of a cook the oven doesn't stop.** It stays on
  at whatever it was last set to. Anova's cloud doesn't stop it either: that's
  the stock behaviour, and "turn off at the end of the timer" is a
  long-standing request on
  [Anova's forum](https://community.anovaculinary.com/t/turn-off-at-end-of-timer/29211).
  An emulator that matches the cloud sends nothing. One that wants to stop the
  oven, or just report the cook as done, detects the end as
  `stageTransitionPendingUserAction: true` **and** `nodes.timer.mode: idle`
  **and** `activeStageIndex` on the last stage. The flag alone isn't enough:
  `START_NEXT_STAGE` on the last stage sets it while the timer is still
  running, so acting on it could end a cook early.

  This only works when the last stage has a timer. **Not covered:**
  - A last stage with **no timer** never makes the timer-ends transition, so
    it never reports done.
  - A last stage that ends on a **probe target**: what the oven then
    reports is untested.

  An emulator that stops the oven itself should refuse such recipes or have
  its own fallback for ending them. Count "the last stage" from the current `cook.stages`, since
  `UPDATE_COOK_STAGES` can add stages mid-cook.
- **Steam is unproven.** The stages started and transitioned (`dry 60°C +
  RH 40%` set `wet.dosed: true` within 10s; `wet 55°C + steam 100%` switched
  `temperatureBulbs.mode` to `wet` and `steamGenerators.mode` to
  `steam-percentage` at once), but no steam was made:
  - The boiler stayed at 0W in both stages.
  - The relative-humidity stage logged `[steam blocked]`, though the cavity was
    already at about 73% humidity against a 40% target.
  - The wet stage logged `steamTarget=100 steamState=0` for over 40s, with the
    boiler at 29°C and `descaleRequired: true`.

  Whether this oven can make steam right now is open; the descale lockout is
  a candidate. Both cooks were stopped after about a minute.

## What an emulator has to do

1. Accept TLS 1.2 on 443, without needing SNI.
2. Complete the WebSocket upgrade; answer the oven's pings.
3. Reassemble fragmented text frames.
4. Send `CMD_APO_HEALTHCHECK` every 10s.
5. Treat `state` as the source of truth: read `mode`, `cook`,
   `processedCommandIds` and `stageTransitionPendingUserAction` from it.
6. Recognise when the final stage has finished: pending flag true, timer idle
   and the active stage the last one (see Running a cook, which also covers
   recipes this test can't detect). Don't send `CMD_APO_STOP`; the cloud
   doesn't, and the oven holds until the user stops it.
7. Send `CMD_APO_SET_REPORT_STATE_RATE` on every connect (the real cloud
   does; the setting may not survive a reconnect).
8. Optionally answer `timezone` with `CMD_APO_SET_TIME_ZONE`. The oven works
   without it, and keeps asking either way.

[tools/oven-cloud-probe.py](../tools/oven-cloud-probe.py) does 1–4 and forwards
whatever is written to its control port. Everything above except the real
cloud's own pre-redirect traffic was observed through it.
