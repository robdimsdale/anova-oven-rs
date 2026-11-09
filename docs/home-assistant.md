# Home Assistant over MQTT

`processors::mqtt` publishes the oven to Home Assistant using MQTT discovery,
and takes commands from it. Home Assistant creates the entities itself: no
Python, no HACS and no Home Assistant core review. It targets one oven and
one Home Assistant instance; it is not meant as a general Anova integration.

It is off unless `ANOVA_MQTT_URL` is set. It reads what the HTTP API serves
(the read model, cook progress and the oven's connection) and sends commands
through the same state-machine paths, so the HTTP API and MQTT can't drift
apart. A broker outage must not take the server down, so it isn't spawned as
critical: it reconnects with backoff, and republishes discovery and the
retained state on every connect.

The topics, and which of them exist only for Home Assistant, are in
[mqtt-ha-split.md](mqtt-ha-split.md); the processor's wiring is in
[architecture.md](architecture.md).

## Why Home Assistant is the phone UI

Anova's phone app can't be pointed at the local server. Once the oven is
redirected (see [oven-cloud-emulation.md](oven-cloud-emulation.md)), the app
loses it, and so do the HACS integrations, since Anova's cloud no longer sees
the oven. Home Assistant's companion app takes its place: it already runs on
the phone, works away from home through whatever remote access the Home
Assistant install has, and can show actionable notifications.

It covers what the phone app is used for day to day, not all of it:

- view saved recipes and their stages;
- start a recipe or a one-stage manual cook, now or later, and stop a cook;
- start the next stage when the oven is waiting for the user;
- adjust a running cook: lamp, timer, temperature;
- get a notification at each milestone: preheat done, stage done, waiting for
  the user, cook done, and anything that failed.

Creating and editing recipes stays out of Home Assistant: the server's web
UI (`/recipes`) does that.

## Local mode and cloud mode

Nothing in the MQTT processor depends on the mode, but what it can offer
does:

| | Cloud mode | Local mode |
|---|---|---|
| Internet or Anova outage | The oven can't be controlled | Nothing changes |
| Did the start work? | Unknown; the cloud doesn't ack | `processedCommandIds` gives a real ack or a timeout |
| Which stage is running | Inferred (`infer_stage_index`) | The oven's own `activeStageIndex` |
| "Stage ready, waiting for you" | Inferred | The oven's own `stageTransitionPendingUserAction` |
| Start the next stage | Rejected as "unauthorized" | `CMD_APO_START_STAGE` works |
| Lamp, timer, temperature mid-cook | Refused | Supported |
| Start later | Refused | Supported |
| Idle state updates | About every 10 minutes | Every 5s, since the server sets the rate |
| Noticing the oven has dropped off | Up to 20 minutes | Within 90s |
| `cook_done` | Never; every end is `cook_stopped` | From the oven's own state |

Next stage, the controls and starting later are announced only in local
mode, so Home Assistant never shows a button the cloud will refuse.

## What it enables

- **Actionable "stage ready" notifications.** When the oven waits for the
  user between stages, the phone gets a notification with a **Next step**
  button, which sends `CMD_APO_START_STAGE`. A cook can pause after a sear
  and carry on from the sofa.
- **Reliable "done" and "stage done" triggers.** They come from the oven's
  own state, not from inference, so automations can trust them.
- **Starts that report failure.** A start, now or scheduled, that isn't
  acked fails visibly with `command_failed`, so an automation can notify
  instead of silently doing nothing.
- **Pre-flight checks before an unattended start**, with the oven's state at
  most 5s old: connected, door closed, water tank not empty. (Door and tank
  readings are unconfirmed; see Open questions.)
- **Turning the oven off after the cook.** The oven holds at its last
  setting until stopped. A recipe's last stage can turn it off (set in the
  web UI), or an automation can stop it some time after `cook_done`.
- **No dependency on Anova's account.** No token refresh failing at 6pm, and
  no changes on Anova's side breaking dinner.

## Entities

One device, "Anova Oven". The full set, with every `unique_id`, is pinned
by `crates/anova-oven-server/testdata/mqtt-discovery.json`.

- **Sensors:** mode, temperature, target temperature, wet bulb, probe,
  steam, timer remaining, current recipe, step, step description, **step
  ends at** (a timestamp, so the phone counts down without a state update
  every second), firmware, selected recipe, manual cook, scheduled recipe,
  scheduled start.
- **Binary sensors:** connected, cooking, door, water tank empty, next step
  ready, descale required, problem (the oven's fault flags, listed in its
  `faults` attribute).
- **Selects:** recipe, and the manual cook's mode, heating elements,
  preheat, how it ends and what happens at the end.
- **Numbers:** set temperature and set timer (the running cook), the manual
  cook's temperature, steam, timer and probe target, and start in minutes.
- **Text:** start at time (`HH:MM`).
- **Buttons:** start (the selected recipe), start manual cook, start in,
  start at, start scheduled cook now, cancel scheduled start, stop, next
  step.
- **Light:** lamp.
- **Event:** cook, carrying the milestones.

Entity ids are set with `default_entity_id` (`sensor.anova_oven_stage` and
so on), which replaced `object_id` in Home Assistant 2025.10. A change to a
`unique_id` orphans the user's entities, dashboards and automations, so the
snapshot test exists to make one show up in review; regenerate it with
`UPDATE_SNAPSHOTS=1 cargo test -p anova-oven-server discovery` only after
checking none changed.

**Units.** Temperature sensors are published in °C with
`device_class: temperature`; Home Assistant converts them to the user's unit
system, shown to the degree (`suggested_display_precision: 0`). Temperature
numbers (set temperature, and the manual cook's temperature and probe
target) are the exception: they take the oven's display unit, in whole
degrees, with templates converting to and from the °C the server keeps.
Home Assistant's own conversion of a °C number shows fractions
(`160.000006 °F`), and a 1 °C step skips most of °F. Their discovery is
republished when the display unit changes. The timer numbers are in
minutes, sent as seconds through a `command_template`.

**Availability.** An MQTT last will on the server's connection, combined
with the server's own `connected` flag for the oven: if either goes away,
the entities show as unavailable rather than stale. Some entities add a
third topic, with `availability_mode: all`:

- The next step button is available only while `next_stage_ready` is true
  (`next_stage/availability`), so it can't be pressed at the wrong moment,
  and a dashboard can show it only when it means something.
- Start scheduled cook now and cancel scheduled start are available only
  while a cook is scheduled.

## Viewing recipes

A select can only hold option strings, and a dashboard needs the stages too.
So:

- The select's options are recipe titles, sorted. Duplicate titles get a
  suffix (`Bread (2)`) so every option maps to one recipe. The server keeps
  the title → ID map; Home Assistant only ever sees titles.
- The selected recipe lives in the server, so a stale select can't start the
  wrong recipe. It's kept in memory, so it falls back to the first recipe
  after a server restart.
- When the selection changes, the server publishes the recipe to the
  selected recipe sensor's attributes: title, source (own or bookmarked),
  `step_count`, total timed duration, and a `steps` list, counted as the
  recipe editor does (a preheat and its cook are one step). Each step has
  its kind, title, temperature (°C), bulb mode, steam, fan, timer or probe
  target, elements, whether it preheats first, and whether it waits for the
  user.
- Each step also carries a one-line `summary` the server formats, e.g.
  `Bake · 220 °C dry · preheat first · 25 min · waits for you`, in the
  oven's display unit. A markdown card can then list a recipe without
  templating every field.

Home Assistant's recorder stores attributes on every state change, so the
sensor's state is the recipe title and the attributes change only when the
selection or the recipe does.

## Milestone events

Notifications need edges, not states. Home Assistant could derive them by
diffing sensors, but it would have to re-derive the stage logic the server
already has, and a Home Assistant restart replays state, which would notify
again. So the server publishes them to the cook event entity, not retained:

| `event_type` | When |
|---|---|
| `cook_started` | The oven goes from idle to preheat or cook |
| `preheat_done` | The preheat stage ends and the first cook stage begins |
| `stage_started` | Any later stage begins, by timer or by the user |
| `stage_waiting` | A stage is done and the next one waits for the user |
| `cook_done` | The last stage's timer ends; the oven keeps holding until stopped |
| `cook_stopped` | The cook ends before its last stage did |
| `command_failed` | A command was refused, or the oven didn't ack it |
| `oven_lost` | The oven's connection drops while a cook is running |
| `cook_scheduled` | A cook is set to start later, or its time changes |

One edge, one event: a preheat whose next stage waits for the user (to put
the food in) ends with `stage_waiting`, and `preheat_done` or
`stage_started` follows only when the user starts the stage.

Each payload carries `event_type`, `cook_id`, `recipe_title`, `step_index`,
`step_count`, `current_step` and `next_step` (summaries), and
`next_step_prompt` (the note while it waits), all counting steps as the
recipe editor does. `command_failed` adds the command and the reason;
`cook_scheduled` carries `recipe_title`, `recipe_id` and `at`. `cook_id` is
the oven's `cookId`, so a notification can use it as a tag, and each
milestone replaces the previous one for that cook instead of piling up.

How they're made:

- **In the MQTT processor**, as a pure function of successive snapshots of
  what `/status` serves (`mqtt::milestones`), not in the state machine:
  cook progress lives in `CookProgressTask`.
- **The first snapshot after the server starts is a baseline**, so a
  restart mid-cook announces nothing, in either mode, with no persistence.
- **Cooks are keyed by the oven's `cookId`**, not the current cook's start
  time, which in local mode moves once (from the state machine's
  placeholder to the journal's) and fired `cook_started` twice.
- **`command_failed` comes from two places:** a command Home Assistant sent
  that was refused at once (bad payload, not cooking, cloud mode), and the
  state machine's failed acks, for start, stop and every control, whoever
  sent them. Button presses have no return value, so this is the only way a
  failed start reaches the phone. A press that succeeds needs no event; the
  state change and the next milestone show it.

## Configuration

| Variable | Default | |
|---|---|---|
| `ANOVA_MQTT_URL` | unset | e.g. `mqtt://host:1883`; unset turns MQTT off |
| `ANOVA_MQTT_USERNAME`, `ANOVA_MQTT_PASSWORD` | unset | |
| `ANOVA_MQTT_TOPIC_PREFIX` | `anova/oven` | |
| `ANOVA_MQTT_DISCOVERY_PREFIX` | `homeassistant` | Home Assistant's default |
| `ANOVA_MQTT_CLIENT_ID` | `anova-oven-server` | a second server on the same broker needs its own |

## Security decisions

Decided 2026-10-15.

- **No TLS to the broker.** The broker is on the home network; plain
  `mqtt://` with the broker's credentials is enough.
- **No auth on the HTTP API**, `/start` and `/stop` included. Anything on
  the LAN can drive the oven through the server. That's accepted for a home
  network, and it sets the bar for the next item: there's little point
  guarding the oven's connection more tightly than the HTTP API.
- **Authenticating the oven's connection: deferred.**

  *The exposure.* The device listener reads the JWT's `deviceId` without
  checking the signature, and a new connection always replaces the current
  one. So anything on the LAN can connect to 443 with a made-up token and,
  while connected, feed in fake state (door, tank, `cook_done`,
  `stage_waiting`, which Home Assistant and the pico would act on) and
  receive the commands meant for the oven. The real oven reconnects within
  about 2s and displaces it, so the two would fight. It can't command the
  oven itself: the oven only listens to its own connection. Given the open
  HTTP API, the extra risk is misleading readings and a flapping
  connection, not control of the oven.

  *Why pinning the oven's ES256 public key on its first connection doesn't
  work as it sounds:*
  - The JWT carries no public key. ECDSA lets a key be recovered from a
    signature, but one signature leaves 2–4 candidates, and a second,
    different signature is needed to pick one.
  - The oven signs one token and reuses it for its whole 24-hour life: both
    connections in the probe log, 3.5 minutes apart, sent the identical
    token. A second signature only comes after a reboot or a day, and even
    with the key pinned, a copied token replays until it expires. (Copying
    it means being the TLS server the oven connects to, e.g. by winning its
    DNS answer for `iot-prod`.)

  *Options, cheapest first, for when it's picked up:*
  1. **Allow only the oven's address.** A DHCP reservation for the oven
     and an `ANOVA_DEVICE_ALLOW` setting with its IP; the listener drops
     anything else. Stops casual impersonation; beating it needs ARP
     spoofing, and an attacker who can do that can already do anything on
     the LAN. A few lines of code.
  2. **Ask the oven for a TLS client certificate.** It has never been asked
     (the probe and the listener request none). Secure elements like the
     oven's are often provisioned with a device certificate. If it presents
     one, pin that certificate's key: TLS then proves the oven holds the
     private key on every connection, with no replay window. Try it first
     as an optional client-certificate request in the probe, logging what
     comes back, at the next power cycle.
  3. **Recover the key from token signatures** and verify after that.
     Trustworthy only once two distinct tokens have been seen (a reboot or a
     day), still replayable for up to 24 hours, and buys little given the
     open HTTP API. Least attractive.

  Revisit if the server becomes reachable from outside the home network,
  if the HTTP API gets auth, or before relying on automations that act on
  the oven's readings (pre-flight checks, stopping after `cook_done`).

## Open questions

- **Door and water tank aren't confirmed.** What the oven reports when the
  door opens or the tank runs empty is untested
  ([oven-cloud-emulation.md](oven-cloud-emulation.md#open-questions)). Check
  this before relying on them in pre-flight checks.
