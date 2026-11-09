# Separating the MQTT API from the Home Assistant adapter

Future work, not started. The MQTT processor (`crates/anova-oven-server/src/processors/mqtt`)
was built as "the oven in Home Assistant", and Home Assistant's needs have
shaped its topics, not just its discovery configs. This doc records how, what
the usual pattern is, and the split that would follow it.

Read this when:

- Adding a topic or command to the MQTT processor: decide which side it
  belongs on first.
- Adding a second integration (Homie/openHAB discovery, a Node-RED flow).
- Doing the split.

## 1. What's there now

Topics are under `ANOVA_MQTT_TOPIC_PREFIX` (default `anova/oven`).

Usable by any MQTT client:

| Topic | What |
|---|---|
| `availability` | The server's last will: `online` / `offline` |
| `state` | The `/status` JSON, plus `connected`, `phase`, `recipe_title`, `timer_remaining_secs`, `stage_ends_at` |
| `event` | Milestones: `cook_started`, `preheat_done`, `stage_started`, `stage_waiting`, `cook_done`, `cook_stopped`, `oven_lost`, `command_failed`, `cook_scheduled` |
| `cmd/start` | `{"recipe_id": "…"}` |
| `cmd/start_manual` | `{"title": …, "stages": [...]}`, as `POST /start` takes |
| `cmd/stop`, `cmd/next_stage`, `cmd/lamp`, `cmd/temperature`, `cmd/timer` | The controls (the last four local mode only) |
| `cmd/start_in`, `cmd/start_at` | `{"recipe_id", "minutes"}` / `{"recipe_id", "at", "time_zone"}`: start later (local mode) |
| `cmd/start_scheduled_now`, `cmd/cancel_scheduled_start` | The cook set to start later (local mode) |

There only because of Home Assistant:

| Topic | Why it exists |
|---|---|
| `recipes` | Recipe titles, with suffixes for duplicates (`Bread (2)`), because a select's options are strings |
| `recipe/selected`, `recipe/selected/attributes`, `cmd/select_recipe` | A selection kept on the server so the select has a state, and the dashboard has the selected recipe's steps and summaries |
| `cmd/start` with any other payload | Starts that server-side selection (a button's `PRESS`) |
| `manual`, `cmd/manual/<field>` | A one-stage manual cook built up field by field, because entities can't edit a structured cook |
| `cmd/start_manual` with any other payload | Starts that draft |
| `next_stage/availability` | `online` only while a stage waits, so the Next step button shows only then |
| `start_later`, `cmd/start_later/<field>` | The values the Start in and Start at buttons use, kept on the server like the manual draft |
| `cmd/start_in`, `cmd/start_at` with any other payload | Start the selected recipe with those values |

Plus, Home Assistant only: the discovery configs under
`ANOVA_MQTT_DISCOVERY_PREFIX`, and their templates (the `"None"` that
`payload_reset` reads as unknown, the °C ↔ °F conversion of the temperature
numbers).

The module docs say "for Home Assistant" throughout (`mqtt.rs`,
`milestones.rs`, `recipes.rs`, `manual.rs`). For `milestones.rs` that's only
wording; the events are generic.

## 2. The usual pattern

Most MQTT bridges for home-automation devices (zigbee2mqtt, Valetudo,
ESPHome, OpenMQTTGateway) have two layers:

1. **One neutral topic API.** State as JSON and commands on their own
   topics, documented as the contract. Scripts and Node-RED use it directly.
2. **Discovery adapters on top, each optional.** Home Assistant discovery;
   Valetudo also has Homie for openHAB. An adapter publishes descriptions
   that point at the neutral topics and adds few topics, or none, of its own.

A second integration then means a second adapter, not a second API, and most
consumers (Node-RED, scripts) need no adapter at all because they don't do
discovery. Putting one consumer's UI state on the shared namespace is what
doesn't scale. zigbee2mqtt did it, and later had to deprecate the
Home Assistant-shaped parts of its API in a breaking release.

## 3. The split

1. **Modules.** `processors/mqtt` keeps the client, the neutral topics and
   the commands. `processors/mqtt/homeassistant` gets discovery, the recipe
   selection (`recipes.rs`), the manual draft (`manual.rs`) and
   `next_stage/availability`.
2. **Topics.** The Home Assistant-only topics move under their own prefix,
   e.g. `anova/oven/ha/recipes`, `anova/oven/ha/cmd/manual/<field>`.
   Discovery points at the new paths, so Home Assistant follows on the next
   republish. `cmd/start` and `cmd/start_manual` without a JSON body (the
   buttons' `PRESS`) become `ha/cmd/start_selected` and `ha/cmd/start_draft`,
   so the neutral commands always take a full payload.
3. **Optional discovery.** An empty `ANOVA_MQTT_DISCOVERY_PREFIX` (or a
   separate switch) turns the adapter off, leaving the plain API.
4. **Docs.** The neutral topics get their own section in
   `home-assistant.md` (or a doc of their own) as the contract, apart
   from the Home Assistant section.

What stays the same: every `unique_id` and `default_entity_id`, so
Home Assistant entities, dashboards and automations keep working. The
snapshot test (`testdata/mqtt-discovery.json`) will show the topic paths
changing and nothing else.

## 4. Keeping the Home Assistant state on the server

The selection and the draft could leave the server: Home Assistant helpers
(`input_select`, `input_number`) and a script that builds the
`cmd/start_manual` JSON. That's purer, but every Home Assistant user would
then need a package or blueprint, losing zero-configuration discovery. The
split fences that state off rather than removing it.

## 5. Until then

New topics and commands go on the neutral side unless they exist only for a
Home Assistant entity, in which case they're the adapter's, and should be
written so they can move under `ha/` without changing their payloads.
