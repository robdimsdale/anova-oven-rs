# Plan: running the oven fully offline

A plan for extending `anova-oven-server` so it stands in for Anova's cloud and
the oven can be controlled with no internet access at all. It builds on
[oven-cloud-emulation.md](oven-cloud-emulation.md), which describes the
device-side protocol.

Status: **Phases 0–4 done**. Against the real oven (2026-10-16): the exit
test (Phase 1) passed, a real start ran a three-stage recipe to the end
(Phase 2), and `export-recipes` wrote all 12 recipes (Phase 3). Still to do
against the real one: `/stop` from the server (that cook was stopped at the
oven's panel) and the controls (Phase 4). Recipes can now be edited in the
server's web UI, with the JSON API moved under `/api`; see
[architecture.md](architecture.md).

## Where we are

The server today depends on Anova's cloud in two places:

- **Oven control.** `processors::ws` is a *client* of the app-side API at
  `devices.anovaculinary.io`. Anova's cloud relays commands to the oven and
  state back.
- **Data.** Recipes, history and the current cook all come from Firestore
  (`processors::firestore`). The server also needs a Firebase sign-in at
  startup (`ANOVA_EMAIL`/`ANOVA_PASSWORD`), so it won't start without the
  internet.

To run offline, the server has to become the *device-side* endpoint the oven
connects to, and replace Firestore with local storage.

The state machine talks to the oven only through `WsCommand` and `WsEvent`
([runtime/types.rs](../crates/anova-oven-server/src/runtime/types.rs)). That
contract is where the transport is swapped: a new device-side processor emits
the same events, and the reducers barely change.

## Target shape

```
oven ──wss:443 (self-signed, DNS-redirected)──► device processor ─┐
                                                                  ├─► state machine ─► HTTP API (unchanged)
                     local store (recipes, history, current cook) ◄┘
```

A setting `ANOVA_MODE=cloud|local` chooses the mode at startup:

- **`cloud`** (the default) behaves exactly as today.
- **`local`** listens for the oven, uses the local store, and needs no Anova
  credentials.

The HTTP API that the CLI and pico use doesn't change, apart from the new
endpoints in Phase 4.

## Phase 0: refactors with no behaviour change

*Done.*

These make room for a second transport and a second store. Existing tests
should pass unchanged.

1. **Share the state parser.** The device's `state` message carries `data`
   with the same shape as the app-side `payload.state`: `{state, nodes, cook,
   …}`. Split [protocol.rs](../crates/anova-oven-server/src/protocol.rs) so
   both paths deserialize into `OvenState` and share `to_oven_status`.
   Extend:
   - `Cook` with `stages`, `stageTransitionPendingUserAction`,
     `secondsElapsed` and `activeStageSecondsElapsed`;
   - `StateInfo` with `processedCommandIds`.
2. **Separate the stage JSON from the envelope.** In
   [ws.rs](../crates/anova-oven-server/src/processors/ws.rs),
   `start_command_json` builds the stages inside the app-side envelope
   (`{command, payload: {type, id, payload}, requestId}`). Pull the stage
   serialization out so the device envelope (`{id, type, payload}`) can reuse
   it. `update_cook_stages_command_json` gets the same treatment.
3. **Put the Firestore processor behind a store interface.** Rename
   `FirestoreCommand`/`FirestoreEvent` to `StoreCommand`/`StoreEvent`, so a
   local implementation can answer the same requests. `SmError::Firestore`
   becomes `SmError::Store`.

## Phase 1: device listener, read-only

*Done, apart from the exit test.* As built, beyond what's below:

- `ANOVA_MODE=local` runs `processors::device` in place of `processors::ws`,
  and needs no credentials. Until Phase 3, an empty store stood in for
  Firestore: no recipes, no history and no current cook.
- `/start` and `/stop` were logged and dropped until Phase 2.
- `ANOVA_TIMEZONE` falls back to the system's zone. With neither,
  `timezone` requests go unanswered.
- A connection with no readable `deviceId` is accepted with the cooker ID
  `unknown`, rather than locking out an oven whose JWT changes.
- `ANOVA_WS_READ_TIMEOUT_SECS` defaults to 90s in local mode. Hitting it
  drops the oven's connection, and the oven reconnects.
- A fake-oven test (`processors/device/tests.rs`) covers TLS 1.2 with
  `ECDHE-RSA-AES256-GCM-SHA384` and no SNI, the JWT, a fragmented `state`,
  the report rate, healthchecks, `timezone`, replacement by a new connection
  and the state timeout.

A new `processors::device` doing items 1–8 of
[What an emulator has to do](oven-cloud-emulation.md#what-an-emulator-has-to-do).

**TLS**
- tokio-rustls, listening on `ANOVA_DEVICE_BIND` (default `0.0.0.0:443`).
- It must offer TLS 1.2 with `ECDHE-RSA-AES256-GCM-SHA384` and must not
  require SNI.
- A fresh self-signed certificate is generated with rcgen at every startup and
  held only in memory. There's no key or certificate to store, configure or
  rotate.
- The key must be RSA, because the oven negotiated `ECDHE-RSA-…`, and whether
  it offers any ECDSA suites is unknown. rcgen's default key is ECDSA P-256,
  and it can only generate RSA keys with its `aws_lc_rs` feature. Generating a
  2048-bit key takes well under a second at startup.

**WebSocket**
- tokio-websockets' `server` feature, which already answers pings and
  reassembles fragmented frames (the first `state` after connecting arrives
  in fragments).
- Read the `Authorization` JWT and decode `deviceId` without verifying the
  signature. It takes the place of the cooker ID, so the processor emits
  `WsEvent::CookerDiscovered { cooker_id: deviceId }`.
- One oven at a time. A new connection replaces the old one, which is what a
  reconnect looks like.

**Session duties**
- On connect, send `CMD_APO_SET_REPORT_STATE_RATE {"idle": 5, "cooking": 5}`
  (cooking 10 until 2026-10-16; 5 keeps the status page and panels closer to
  live). Idle was 30 until 2026-10-17: a command is confirmed only by the next
  report, and the lamp, fan, vent and probe commands don't always prompt one,
  so a lamp toggled from Home Assistant showed as off for ~30s and its ack
  timed out.
  Whether the oven honours 5 is to be seen; see the open question below.
  The doc says to send it on every connect, since the setting may not survive
  a reconnect.
- Send `CMD_APO_HEALTHCHECK` every 10s.
- Answer `timezone` with `CMD_APO_SET_TIME_ZONE`, built from an IANA zone
  name in `ANOVA_TIMEZONE`. Compute `gmt_offset` from the zone's offset now,
  including DST, which avoids the documented hour-off bug from a fixed
  `PST`/`-28800`.
- Log `pairing` and `diagnostic` messages. Neither needs a reply.

**Liveness**

The existing `Liveness` tracker and read timeout carry over. With idle reports
every 30s and a healthcheck reply every 10s, the 1200s default
(`ANOVA_WS_READ_TIMEOUT_SECS`) can drop to about 60–90s in local mode, so a
lost oven shows up on the pico within a minute and a half.

**Exit test:** point DNS at the server, power-cycle the oven, and see
`/status` update with no internet connection. Then restart the server without
power-cycling the oven, and check that the oven reconnects under the new
certificate. The probe reused one certificate throughout, so this hasn't been
seen yet.

## Phase 2: commands with real acks, and the end of a cook

*Done.* As built, beyond what's below:

- The command `id` is the state machine's request ID, so an ID in
  `processedCommandIds` maps straight back to the pending start or stop. The
  ack is emitted after the `ApoState` of the same `state`, so the state
  machine sees the oven's new state first.
- **Timeouts live in the device processor, not `PendingTimeout`.** It alone
  knows when a command actually went out. A start or stop not seen within
  10s is reported as `CommandAck { status: "timeout" }`, and one sent with no
  oven connected as `"disconnected"`. The state machine's existing failed-ack
  paths then roll back: a start to `Idle`, a stop to `Cooking`. A later
  `state` puts it right if the oven did act. `TickKind::PendingTimeout` is
  still unused.
- Unacked commands survive a reconnect, since the oven may have handled one
  just before the connection dropped.
- In local mode an `ok` start ack also confirms a start that replaced a
  running cook. That has no idle-to-cooking edge, so it used to stay
  `StartPending`.
- The end of a cook is served as `cook_done` in `/status` (omitted when
  false). It's computed in `protocol::cook_done` from the oven's own state,
  so it's only set in local mode in practice.
- With no recipes until Phase 3, `/start` couldn't run end to end in local
  mode then.
- A store can answer `FetchCurrentCook` with `StoreError::Unsupported`, and
  the state machine keeps the cook it already has.
- Tested with a fake oven (start, stop, acks, timeout, no oven) and
  `cook_done` against the captured `state`, including the cases that mustn't
  count as done.

**Start and stop over the device envelope**

`POST /start` and `POST /stop` send `CMD_APO_START`/`CMD_APO_STOP` as
`{id, type, payload}`.

**Real acks**

The oven lists every command it has handled in `processedCommandIds`. When a
command's `id` appears in a later `state`, emit `WsEvent::CommandAck { status: "ok" }`.
If it hasn't appeared after a few seconds, fail it through the existing
`PendingTimeout`. The state machine's pending tracker already handles both
cases, so this closes the "start/stop are fire-and-forget" gap listed in
[architecture.md](architecture.md#known-gaps-and-gotchas).

How long an ID stays in the list depends on both sides:

- **The oven keeps only the last 10 IDs.** This was seen in captured `state`
  messages, and the server can't change it.
- **The server's healthchecks fill it, one every 10s.** That interval is our
  choice (Phase 1), copied from the probe. Every command lands in the list,
  healthchecks included.

So an ID drops out after about 100s (10 slots × 10s), sooner if other commands
are sent. That's ample: the oven sends a `state` within about 10s of handling
a command, and the server reads every one. Only a missed `state` makes an ID
age out before it's seen. A longer healthcheck interval, or pausing
healthchecks while a command awaits its ack, would widen the window if it ever
matters.

**No auto-stop at the end of a cook**

When the last stage's timer ends, the oven stays on at whatever it was last
set to, until someone stops it. Anova's cloud doesn't stop it either. That's
the stock behaviour users see, and "turn off at the end of the timer" is a
long-standing request on Anova's forum
([thread](https://community.anovaculinary.com/t/turn-off-at-end-of-timer/29211);
see also Anova's [hardware FAQ](https://support.anovaculinary.com/hc/en-us/articles/360003919912-Hardware)).
Local mode copies it: the server never sends `CMD_APO_STOP` on its own. The
user stops the cook, from the oven, the CLI or Home Assistant.

The server still has to recognise the end of a cook, for the `cook_done`
milestone and for `/status`. That's when all three hold:

- `stageTransitionPendingUserAction` is true,
- `nodes.timer.mode == "idle"`,
- `activeStageIndex` is the last entry of the current `cook.stages`.

The pending flag alone isn't enough: `START_NEXT_STAGE` on the last stage sets
it while the timer is still running, so it would report the cook done early.
Count "the last stage" from the oven's `cook.stages`, since
`UPDATE_COOK_STAGES` can add stages mid-cook.

A last stage with no timer, or one that ends on a probe target, never meets
these conditions, so it never reports done. That's harmless without an
auto-stop: the oven holds, as it would in cloud mode.

"Turn off when done" as a recipe setting is out of scope for now. Anova's
recipes have no such field, and adding it to locally stored recipes would
mean merging two sources of truth for the same recipe.

## Phase 3: a local store in place of Firestore

*Done, apart from running `export-recipes` against the real account.* As
built, beyond what's below:

- `processors::local_store` replaces the empty store. `ANOVA_DATA_DIR`
  defaults to `anova-oven-data` in the working directory.
- `export-recipes` is a subcommand of the server
  (`anova-oven-server export-recipes`), since the Firestore code lives
  there. It writes `recipes/<id>.json`, replacing files with the same ID
  and leaving others alone. Recipes are served own first, then bookmarked, each sorted by title; a file that
  doesn't parse is skipped with a warning.
- **The oven's cook reaches the store.** Each `state`'s `cook` block goes
  to the state machine as `OvenCook` (cookId, stages parsed with the same
  `stage_from_json` as Firestore's, seconds elapsed), and
  `StoreCommand::FetchCurrentCook` carries it. The current cook is that,
  plus title, recipe ID and start time from `active-cook.json`.
- `/start` writes `active-cook.json` when the state machine's
  `PatchCookRecipeRef` arrives, the same command that tags the cook with its
  recipe in Firestore.
- **A cook from the oven's panel** has no matching `active-cook.json`. The
  store records it as `[manual]` on first sight, with the start time worked
  back from `secondsElapsed`. Storing it keeps the start time fixed, which
  matters because the start time keys the cook in `CookProgressTask`.
- **A start over a running cook:** while a start is pending, the state
  machine doesn't pass the oven's cook on, since it's the one being
  replaced. Otherwise the store would record it as `[manual]` over the new
  cook's record.
- **`active-cook.json` is a journal of the cook in flight**, like the
  pico's persisted memory. Every oven report reaches the store as
  `StoreCommand::OvenReported` (the oven's cook, or none), and the store
  reconciles the journal against it:
  - the same cook: rewrite `last_seen_at` in place;
  - a different cook, or none: the journalled cook has ended. It goes to
    `history.jsonl`, ended now if this process watched it to the end, or at
    `last_seen_at` if not, and the journal moves on to the oven's cook (as
    `[manual]`) or is removed.

  So a cook that ends while the server is down is found on the first report
  after a restart, and dated within one report interval (10s cooking) of
  its real end. The cost is one small in-place write per report while
  cooking; nothing accumulates. A start the oven never reported has no
  `last_seen_at` and is dropped without a history entry. A start over a
  running cook ends the old one first. The state machine holds reports back
  while a start is pending, since the oven's cook (or idleness) then is
  about to be replaced. When a cook goes to history the store sends a fresh
  history unprompted, so one found at startup shows in `/history` at once.
  Firestore ignores `OvenReported`.
- `GET /history` returns the last 50 entries, as the Firestore store does.

All under `ANOVA_DATA_DIR`.

- **Recipes:** `recipes/*.json`, one `anova_oven_api::Recipe` per file. A
  one-off `export-recipes` command signs in to Firestore while the internet is
  still available and writes the user's own and bookmarked recipes out.
  `POST /update-recipes` re-reads the directory. Recipes are created and
  edited in the server's web UI (`/recipes`), or by hand.
- **Current cook:** the oven's own `cook` block (cookId, stages, active index)
  is the authority. The server adds what the oven doesn't know (recipe title,
  recipe ID, start time) from `active-cook.json`, written on `/start` and
  removed on stop. A server restart mid-cook then keeps the title. A cook the
  server didn't start (e.g. from the oven's own panel) shows as `"[manual]"`,
  as today.
- **History:** when the oven goes from cooking to idle, append an entry to
  `history.jsonl`. `GET /history` reads it newest first.

## Phase 4: features the cloud blocked

*Done, against a fake oven.* As built, beyond what's below:

- One state-machine command, `Control`, carries every client request
  (`ControlRequest`: next stage, timer, temperature, lamp, unit). It
  becomes an `OvenControl` for the device processor, which sends and
  tracks it like start and stop. Cloud mode refuses all of them with `501`.
- `/next-stage` works whether or not the oven is waiting, so it can also cut
  a stage short. It's refused on the last stage.
- **Preheat edits** go out as `UPDATE_COOK_STAGES` built from the oven's own
  stage JSON (`OvenCook::raw_stages`), edited in place, with `stepType` and
  `stageTransitionType` added back as in the probe's update. Nothing is lost
  in a round trip through `Stage`, and the ids stay as the oven has them.
  `/temperature` edits the preheat; `/timer` edits the stage after it,
  whose timer the oven shows during a preheat. The lamp and display unit
  aren't stage settings, so they always go out as `SET_*`.
- `/status` carries `stage_transition_pending_user_action`. In local mode
  `CookProgressTask` takes the stage from `activeStageIndex` and
  `next_stage_ready` from that flag, with a stage still to come (on the last
  stage the flag means done). Cloud mode keeps the inference.
- **Pico:** pressing the encoder on the "Next stage?" screen sends
  `POST /next-stage`. The screen stays until the server clears
  `next_stage_ready`. Under cloud mode the press gets a `501`, logged.
- A failed control ack is logged; there's nothing to roll back, since the
  next `state` shows what the oven did.
- **CLI:** `next-stage`, `timer`, `temperature` (needs a unit: `180c` or
  `356f`), `lamp`, `temperature-unit` and `update-recipes`; `status` says
  when the oven is waiting for you or done.

**Stage advance**

`CMD_APO_START_STAGE` and `CMD_APO_START_NEXT_STAGE` both worked through the
probe, whereas the cloud rejected `START_STAGE` as "unauthorized"
([start-stage-unauthorized-debug.md](exploration/start-stage-unauthorized-debug.md)).
So, in local mode:

- Add `POST /next-stage`, sending `CMD_APO_START_STAGE` with the next stage's
  `stageId`.
- Wire the pico's `next_stage_ready` prompt to a button press.
- Switch [cook_progress.rs](../crates/anova-oven-server/src/cook_progress.rs)
  from inferring the stage (`infer_stage_index`, `evaluate_stage_completion`)
  to the oven's own `activeStageIndex` and `stageTransitionPendingUserAction`.

**Other controls**

Endpoints for `SET_TIMER`, `SET_TEMPERATURE_BULBS`, `SET_LAMP` and
`SET_TEMPERATURE_UNIT`, plus CLI subcommands. During a preheat, `SET_*` copies
the preheat's settings onto the paired cook stage, so the server should send
`UPDATE_COOK_STAGES` with the edited stage list instead while a preheat is
active.

## Phase 5: confirm it works with no internet

- Block the oven's internet access at the router.
- Run a full multi-stage cook end to end: preheat, a stage that auto-advances,
  a stage that waits for the user, and `cook_done` at the end with the oven
  still holding.
- Update [architecture.md](architecture.md) and
  [oven-cloud-emulation.md](oven-cloud-emulation.md) with the results.
- Retire [tools/oven-cloud-probe.py](../tools/oven-cloud-probe.py), or keep it
  as a reference tool.

## Home Assistant over MQTT

*Built, not yet connected to Home Assistant.* `processors::mqtt` does
everything below. The security items under "What it needs" were decided
against or deferred (see [Security decisions](#security-decisions)). It is
off unless `ANOVA_MQTT_URL` is set. Tested against a fake
broker and fake oven: discovery, selecting and starting a recipe over MQTT,
`stage_waiting` and the next stage button's availability, next stage,
`cook_stopped`, and `command_failed` for both a bad payload and an ack
timeout. As built, beyond what's below:

- **Milestones are worked out in the MQTT processor**, from successive
  snapshots of what `/status` serves, not in the state machine: cook
  progress (the stage and `next_stage_ready`) lives in `CookProgressTask`,
  not the state machine. They're still a pure function
  (`mqtt::milestones`). The first snapshot after the server starts is a
  baseline, so a restart mid-cook announces nothing, in either mode, with
  no persistence.
- **Cooks are identified by the oven's `cookId`**, now in `/status` as
  `cook_id`, not by the current cook's start time: in local mode that moves
  once, from the state machine's placeholder to the journal's, which fired
  `cook_started` twice. The events' `cook_id` is the same value.
- `command_failed` comes from two places: a command Home Assistant sent
  that was refused at once (bad payload, not cooking, cloud mode), and the
  state machine's failed acks, which it now announces for start, stop and
  every control (whoever sent them, HTTP included).
- In cloud mode `cook_done` is never set, so every end of a cook is
  `cook_stopped`.
- The state topic carries the `/status` JSON plus `connected`, `phase`,
  `recipe_title`, `timer_remaining_secs` and `stage_ends_at`. The timer
  number is in minutes, sent as seconds through a `command_template`.
- The selected recipe is kept in memory, so it falls back to the first
  recipe after a server restart.
- Entity ids are set with `default_entity_id` (`sensor.anova_oven_stage`
  and so on), which replaced `object_id` in Home Assistant 2025.10.

The server publishes to MQTT using Home Assistant's discovery format, and Home
Assistant creates the entities itself. No Python, no HACS and no HA core
review. It targets one oven and one Home Assistant instance; it is not meant
as a general Anova integration.

### Why Home Assistant is the phone UI

Anova's phone app can't be pointed at the local server. Once the oven is redirected, the app
loses it. Home Assistant's companion app is the replacement: it already runs
on the phone, works away from home through whatever remote access the Home
Assistant install has, and can show actionable notifications.

The aim is to cover what the phone app is used for day to day, not all of it:

- view saved recipes and their stages;
- start a recipe, and stop a cook;
- start the next stage when the oven is waiting for the user;
- get a notification at each milestone: preheat done, stage done, waiting for
  the user, cook done, and anything that failed.

Creating and editing recipes stays out of Home Assistant: the server's web
UI does that (see Phase 3).

### Why local mode changes the picture

In cloud mode the server is itself a client of Anova's cloud. A Home Assistant
integration on top of it could only be as good as that relay: start and stop
a recipe, fire-and-forget, with stage progress guessed from temperatures and
timers. Local mode is what makes the rest worth building.

| | Cloud mode | Local mode |
|---|---|---|
| Internet or Anova outage | The oven can't be controlled | Nothing changes |
| Did the start work? | Unknown; the cloud doesn't ack | `processedCommandIds` gives a real ack or a timeout |
| Which stage is running | Inferred (`infer_stage_index`) | The oven's own `activeStageIndex` |
| "Stage ready, waiting for you" | Inferred | The oven's own `stageTransitionPendingUserAction` |
| Start the next stage | Rejected as "unauthorized" | `CMD_APO_START_STAGE` works |
| Lamp, timer, temperature mid-cook | Not wired up | Phase 4 controls |
| Idle state updates | About every 10 minutes | Every 30s, since we set the rate |
| Noticing the oven has dropped off | Up to 20 minutes | About 60–90s |
| Anova account in the server | Required, plus a token session | None |

The HACS integrations stop working once the oven is redirected, since Anova's
cloud no longer sees it, so the server is the only route into Home Assistant
anyway.

### Ship it in cloud mode first

Nothing in the MQTT processor depends on local mode. It reads the state
machine's read model and sends commands through the same paths as the HTTP
API, both of which exist today. So it can ship before Phase 1, with what cloud
mode supports: state, the recipe list and details, start, stop, and milestone
events from the inferred stage progress. Next stage and the Phase 4 controls
are announced only in local mode, so Home Assistant never shows a button the
cloud will refuse.

This splits the risk. Home Assistant becomes the phone UI while the phone app
still works as a fallback, and the cut-over to local mode later changes how
good the data is, not what the user has to learn.

### What it enables

Things that weren't practical before:

- **Actionable "stage ready" notifications.** When the oven waits for the
  user between stages, the phone gets a notification with a **Start next
  stage** button. Pressing it calls `CMD_APO_START_STAGE`. So a cook can
  pause after a sear and carry on from the sofa.
- **Reliable "done" and "stage done" triggers.** They come from the oven's own
  state, not from inference, so automations can trust them: notify when the
  cook ends, or flash a light when a stage finishes.
- **Timed starts that report failure.** "Start recipe X at 17:30" was already
  possible. Now a start that isn't acked within a few seconds fails visibly,
  and the automation can notify instead of silently doing nothing.
- **An optional stop after the cook is done.** The server doesn't stop the
  oven at the end of a cook (Phase 2). A user who wants that can write an
  automation: on `cook_done`, notify, and send stop if nobody has stopped it
  within some time. It lives in Home Assistant, not in the recipe.
- **Pre-flight checks before an unattended start.** Check that the oven is
  connected, the door is closed and the water tank isn't empty, and ask for
  confirmation on the phone if anything is off. With a 5s idle report rate,
  these values are recent enough to rely on. In cloud mode they could be up
  to 10 minutes old.
- **Mid-cook adjustments from the phone.** Lamp on to look in, add ten
  minutes to the timer, drop the temperature.
- **No dependency on Anova's account.** No token refresh failing at 6pm, and
  no changes on Anova's side breaking dinner.

### Entities

One device, "Anova Oven", with:

- **Sensors:** mode, temperature, target temperature, wet bulb, probe, steam,
  timer remaining, recipe title, current stage, stage description, and **stage
  ends at** (a timestamp, so the phone shows "in 12 minutes" and counts down
  without a state update every second).
- **Binary sensors:** connected, cooking, door open, water tank empty, next
  stage ready, descale required.
- **Select:** recipe, filled from the recipe list. It updates by itself when
  the list changes, so recipe IDs never appear in Home Assistant YAML.
- **Sensor:** selected recipe, whose attributes carry the selected recipe's
  stages (see below).
- **Event:** cook, carrying the milestones (see below).
- **Buttons:** start (the selected recipe), stop, next stage.
- **Light:** lamp (only works mid-cook; the oven ignores it when idle).
- **Numbers:** target temperature, timer.

Temperatures are published in °C with `device_class: temperature`; Home
Assistant converts them to the user's unit system. The oven's own display unit
is a separate setting and doesn't affect what is published.

Availability uses an MQTT last-will on the server's connection, combined with
the server's own `connected` flag for the oven. If either the server or the
oven goes away, the entities show as unavailable rather than stale.

The next stage button has a third availability topic, `online` only while
`next_stage_ready` is true (`availability_mode: all`). It can't be pressed at
the wrong moment, and a dashboard can show it only when it means something.

### Viewing recipes

A select can only hold option strings, and a dashboard needs the stages too.
So:

- The select's options are recipe titles, sorted. Duplicate titles get a
  suffix (`Bread (2)`) so every option maps to one recipe. The server keeps the
  title → ID map; Home Assistant only ever sees titles.
- When the selection changes, the server publishes the recipe to the selected
  recipe sensor's attributes topic: title, source (own or bookmarked), stage
  count, total timed duration, and a `stages` list. Each stage has its kind,
  title, temperature (°C), bulb mode, steam, fan, timer or probe target,
  elements, and whether it waits for the user.
- Each stage also carries a one-line `summary` the server formats, e.g.
  `Cook · 220 °C dry · 30% steam · 25 min · waits for you`. A dashboard
  markdown card can then list a recipe without templating every field. The
  summary uses the oven's display unit, the one place that setting matters.

Home Assistant's recorder stores attributes on every state change, so the
sensor's state is the recipe title and the attributes change only when the
selection or the recipe does.

### Milestone events

Notifications need edges, not states. Home Assistant could derive them by
diffing sensors, but it would have to re-derive the stage logic the server
already has, and an HA restart replays state, which would notify again. So the
server publishes them to an MQTT event entity, not retained:

| `event_type` | When |
|---|---|
| `cook_started` | The oven goes from idle to preheat or cook |
| `preheat_done` | The preheat stage ends and the first cook stage begins |
| `stage_started` | Any later stage begins, by timer or by the user |
| `stage_waiting` | A stage is done and the next one waits for the user |
| `cook_done` | The last stage's timer ends; the oven keeps holding until stopped |
| `cook_stopped` | The cook ends before its last stage did |
| `command_failed` | A start, stop or next stage wasn't acked or was refused |
| `oven_lost` | The oven's connection drops while a cook is running |

One edge, one event: a preheat whose next stage waits for the user (to put the
food in) ends with `stage_waiting`, and `preheat_done` or `stage_started`
follows only when the user starts the stage.

Each payload carries `event_type`, `cook_id`, `recipe_title`, `stage_index`,
`stage_count`, the current and next stages' `summary`, and for
`command_failed`, the command and reason. `cook_id` lets a notification use it
as a tag, so each milestone replaces the previous one for that cook instead of
piling up.

They come from the state machine as a pure function of the previous and new
state, next to the end-of-cook detection, so they can be unit tested the same way. In
cloud mode they follow the inferred stage index, and `stage_waiting` can be
late or missed; in local mode they follow `activeStageIndex` and
`stageTransitionPendingUserAction`.

A server restart mid-cook must not fire `cook_started` again. In local mode
`active-cook.json` says the cook was already known; in cloud mode, compare the
oven's `cookId` with the one the read model last saw.

Button presses in Home Assistant have no return value, so `command_failed` is
the only way a failed start reaches the phone. A press that succeeds needs no
event of its own; the state change and the next milestone show it.

### Topics

```
homeassistant/<component>/anova_oven/<object>/config   discovery, retained
anova/oven/availability                                online/offline, last-will
anova/oven/next_stage/availability                     online/offline, retained
anova/oven/state                                       the /status JSON, retained
anova/oven/recipes                                     sorted title list, retained
anova/oven/recipe/selected                             selected title, retained
anova/oven/recipe/selected/attributes                  the selected recipe, retained
anova/oven/event                                       milestones, not retained
anova/oven/cmd/start         {"recipe_id": "…"}, or empty for the selected recipe
anova/oven/cmd/stop
anova/oven/cmd/next_stage
anova/oven/cmd/select_recipe title
anova/oven/cmd/lamp          ON | OFF
anova/oven/cmd/timer         seconds
anova/oven/cmd/temperature   °C
```

`state` is published on every oven report and after every command. Commands
go through the same state-machine paths as the HTTP endpoints, so the HTTP API
and MQTT can't drift apart. The selected recipe lives in the server, not in
Home Assistant, so it survives an HA restart, and a stale select can't start
the wrong recipe.

### Configuration

| Variable | Default | |
|---|---|---|
| `ANOVA_MQTT_URL` | unset | e.g. `mqtt://host:1883`; unset turns MQTT off |
| `ANOVA_MQTT_USERNAME`, `ANOVA_MQTT_PASSWORD` | unset | |
| `ANOVA_MQTT_TOPIC_PREFIX` | `anova/oven` | |
| `ANOVA_MQTT_DISCOVERY_PREFIX` | `homeassistant` | Home Assistant's default |

### What it needs

- An MQTT broker both the server and Home Assistant can reach, with
  credentials for the server unless both are on loopback.
- An MQTT client in the server (e.g. `rumqttc`), as a new processor alongside
  `processors::http`. A broker outage must not take the server down, so it
  isn't spawned as critical: it reconnects with backoff, and republishes
  discovery and retained state on every reconnect.
- ~~**Pin the oven's key.**~~ Deferred; see below.
- ~~**A token on `/start` and `/stop`.**~~ Not doing; see below.

### Security decisions

Decided 2026-10-15. Nothing here is built.

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

  *Why the original plan ("store the oven's ES256 public key on its first
  connection") doesn't work as written:*
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
  if the HTTP API gets auth, or before relying on Home Assistant
  automations that act on the oven's readings (pre-flight checks,
  stopping after `cook_done`).

### Open questions

- **Door and water tank aren't confirmed yet.** What the oven reports when the
  door opens or the tank runs empty is untested
  ([oven-cloud-emulation.md](oven-cloud-emulation.md)). Check this before
  relying on them in pre-flight checks.
- **How fast a cooking report can be.** `{"cooking": 2}` may not be honoured.
  If it is, temperature graphs in Home Assistant get a lot smoother.
- **A cook started from the oven's panel.** It has no recipe, so the events
  carry `"[manual]"` and the stage list is the oven's own `cook.stages`. Check
  that the panel's cooks report stages the same way.

## Testing

- **Fake oven.** An integration test with a TLS client that skips certificate
  checks, playing the oven's side against the device processor: `pairing`, a
  `state` split into fragments, healthcheck replies, `processedCommandIds`
  echoing sent commands. Frames come from the UART logs.
- **Reducer unit tests,** following the existing ones in
  [state_machine.rs](../crates/anova-oven-server/src/processors/state_machine.rs):
  end-of-cook detection (including the early case it must not trigger on),
  ack matching and timeout, and rebuilding the current cook after a restart.
- **Milestone events,** as reducer tests too: each event fires once on its
  edge, a multi-stage cook produces the expected sequence, and a restart
  mid-cook produces none.
- **Discovery payloads,** as snapshot tests, so a renamed field or a changed
  `unique_id` (which would orphan the user's entities in Home Assistant) shows
  up in review.

## Risks and open questions

- **The phone app stops working while the oven is redirected.** Anova's cloud
  no longer sees the oven, and the app can't be pointed at the local server
  instead. Home Assistant takes its place (see
  [Home Assistant over MQTT](#home-assistant-over-mqtt)). A later option is to
  pass traffic through to the real `iot-prod` alongside local control; out of
  scope here.
- **The oven's clock without internet is untested.** `SET_TIME_ZONE` sets only
  the zone. If the oven gets its time from NTP, blocking the internet may also
  need a local NTP server. Check this during Phase 1.
- **Port 443 needs privileges on Linux.** Use `CAP_NET_BIND_SERVICE` or a port
  redirect. macOS allows it without root.
- **A disconnect during a cook is untested.** Test whether the cook carries on
  through a server restart before relying on it.
- **A firmware update could stop local mode working.** Running
  offline also blocks OTA updates, so the current firmware stays in place.
- **The tokio-websockets server API needs checking.** Confirm the version in
  use exposes the upgrade request's headers, so the JWT can be read; otherwise
  parse the request before handing the stream over.
- **Daylight saving.** Whether the oven applies DST rules from the zone `id`
  or only uses `gmt_offset` is untested. If it's only the offset, the server
  must resend `SET_TIME_ZONE` when DST changes.
