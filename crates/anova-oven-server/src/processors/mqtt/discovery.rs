//! Home Assistant MQTT discovery: one device, "Anova Oven", and its entities.
//!
//! Every config is retained under
//! `<discovery prefix>/<component>/anova_oven/<object>/config`. Entities
//! only local mode can drive (next stage, lamp, set temperature, set timer)
//! get an empty retained config in cloud mode, which removes them, so Home
//! Assistant never shows a control the cloud will refuse.
//!
//! The `unique_id`s are what tie Home Assistant's entities to these configs.
//! Changing one orphans the user's entity, which is why a snapshot test
//! (`testdata/mqtt-discovery.json`) pins the whole set.

use serde_json::{json, Value};

use super::manual;
use super::milestones::EVENT_TYPES;

/// The topics under the server's prefix and Home Assistant's.
#[derive(Clone, Debug)]
pub struct Topics {
    pub prefix: String,
    pub discovery: String,
}

impl Topics {
    fn under(&self, path: &str) -> String {
        format!("{}/{path}", self.prefix)
    }
    pub fn availability(&self) -> String {
        self.under("availability")
    }
    pub fn next_stage_availability(&self) -> String {
        self.under("next_stage/availability")
    }
    pub fn state(&self) -> String {
        self.under("state")
    }
    pub fn recipes(&self) -> String {
        self.under("recipes")
    }
    pub fn selected(&self) -> String {
        self.under("recipe/selected")
    }
    pub fn selected_attributes(&self) -> String {
        self.under("recipe/selected/attributes")
    }
    pub fn manual(&self) -> String {
        self.under("manual")
    }
    pub fn event(&self) -> String {
        self.under("event")
    }
    pub fn command(&self, name: &str) -> String {
        self.under(&format!("cmd/{name}"))
    }
    /// The filter the server subscribes to for commands.
    pub fn commands(&self) -> String {
        self.under("cmd/#")
    }
    fn config(&self, component: &str, object: &str) -> String {
        format!("{}/{component}/anova_oven/{object}/config", self.discovery)
    }
}

/// Who may see an entity as available.
#[derive(Clone, Copy)]
enum Needs {
    /// Only the server (recipes, the event entity, the oven's connection).
    Server,
    /// The server and the oven.
    Oven,
    /// The server, the oven, and a stage waiting for the user.
    WaitingStage,
}

struct Entity {
    component: &'static str,
    object: &'static str,
    name: &'static str,
    local_only: bool,
    needs: Needs,
    /// Component-specific fields.
    fields: Value,
}

/// `(topic, payload)` for every discovery config. `options` are the recipe
/// select's options; `unit` is the oven's display unit (`C` or `F`), which
/// the temperature numbers take.
pub fn configs(
    topics: &Topics,
    local: bool,
    options: &[String],
    unit: &str,
) -> Vec<(String, String)> {
    entities(topics, options, unit)
        .into_iter()
        .map(|e| {
            let topic = topics.config(e.component, e.object);
            let payload = if e.local_only && !local {
                String::new()
            } else {
                config(topics, &e).to_string()
            };
            (topic, payload)
        })
        .collect()
}

/// The recipe select's config alone, republished when the recipes change.
/// It doesn't depend on the unit.
pub fn select_config(topics: &Topics, options: &[String]) -> (String, String) {
    configs(topics, true, options, "C")
        .into_iter()
        .find(|(topic, _)| topic == &topics.config("select", "recipe"))
        .expect("the recipe select is always present")
}

fn config(topics: &Topics, e: &Entity) -> Value {
    let mut availability = vec![json!({ "topic": topics.availability() })];
    if matches!(e.needs, Needs::Oven | Needs::WaitingStage) {
        availability.push(json!({
            "topic": topics.state(),
            "value_template": "{{ 'online' if value_json.connected else 'offline' }}",
        }));
    }
    if matches!(e.needs, Needs::WaitingStage) {
        availability.push(json!({ "topic": topics.next_stage_availability() }));
    }

    let mut config = json!({
        "name": e.name,
        "unique_id": format!("anova_oven_{}", e.object),
        "default_entity_id": format!("{}.anova_oven_{}", e.component, e.object),
        "has_entity_name": true,
        "availability": availability,
        "availability_mode": "all",
        "device": {
            "identifiers": ["anova_oven"],
            "name": "Anova Oven",
            "manufacturer": "Anova",
            "model": "Precision Oven",
        },
        "origin": {
            "name": "anova-oven-server",
            "sw_version": env!("CARGO_PKG_VERSION"),
        },
    });
    let map = config.as_object_mut().expect("an object");
    for (k, v) in e.fields.as_object().expect("an object") {
        map.insert(k.clone(), v.clone());
    }
    config
}

/// A sensor reading `field` from the state JSON, unknown when it's null or
/// absent.
fn reading(topics: &Topics, field: &str, extra: Value) -> Value {
    let mut v = json!({
        "state_topic": topics.state(),
        "value_template": format!("{{{{ value_json.get('{field}') }}}}"),
    });
    for (k, val) in extra.as_object().expect("an object") {
        v[k] = val.clone();
    }
    v
}

fn on_off(topics: &Topics, condition: &str, device_class: Option<&str>) -> Value {
    let mut v = json!({
        "state_topic": topics.state(),
        "value_template": format!("{{{{ 'ON' if {condition} else 'OFF' }}}}"),
    });
    if let Some(class) = device_class {
        v["device_class"] = class.into();
    }
    v
}

/// A select over one of the manual cook's fields.
fn manual_select(topics: &Topics, field: &str, options: &[&str]) -> Value {
    json!({
        "state_topic": topics.manual(),
        "value_template": format!("{{{{ value_json.{field} }}}}"),
        "command_topic": topics.command(&format!("manual/{field}")),
        "options": options,
    })
}

/// A number box over one of the manual cook's fields; `extra` adds the
/// unit.
fn manual_number(
    topics: &Topics,
    field: &str,
    command: &str,
    (min, max): (u32, u32),
    extra: Value,
) -> Value {
    let mut v = json!({
        "state_topic": topics.manual(),
        "value_template": format!("{{{{ value_json.{field} }}}}"),
        "command_topic": topics.command(&format!("manual/{command}")),
        "min": min,
        "max": max,
        "step": 1,
        "mode": "box",
    });
    for (k, val) in extra.as_object().expect("an object") {
        v[k] = val.clone();
    }
    v
}

/// A number box over a temperature the server keeps in °C, `field` of the
/// JSON on `state_topic`, shown in the oven's display unit. In °F the
/// templates convert both ways and round, since Home Assistant's own
/// conversion of a °C number leaves fractions (`160.000006103516 °F`) and
/// a 1 °C step skips most of °F. A null field reads as unknown.
fn temperature_number(
    state_topic: String,
    field: &str,
    command_topic: String,
    (min, max): (u32, u32),
    unit: &str,
) -> Value {
    let get = format!("value_json.get('{field}')");
    let mut v = json!({
        "state_topic": state_topic,
        "command_topic": command_topic,
        "step": 1,
        "mode": "box",
        "device_class": "temperature",
    });
    if unit.eq_ignore_ascii_case("F") {
        let f = |c: u32| f64::from(c) * 1.8 + 32.0;
        v["value_template"] = format!(
            "{{% set c = {get} %}}{{{{ (c * 1.8 + 32) | round | int if c is not none else 'None' }}}}"
        )
        .into();
        // `* 5 / 9`, not `/ 1.8`, so the ends land exactly on the °C limits.
        v["command_template"] = "{{ (value - 32) * 5 / 9 }}".into();
        v["min"] = f(min).ceil().into();
        v["max"] = f(max).floor().into();
        v["unit_of_measurement"] = "°F".into();
    } else {
        v["value_template"] = format!("{{{{ {get} }}}}").into();
        v["min"] = min.into();
        v["max"] = max.into();
        v["unit_of_measurement"] = "°C".into();
    }
    v
}

/// A °C sensor, shown to the degree whatever Home Assistant converts it to.
fn celsius() -> Value {
    json!({
        "device_class": "temperature",
        "unit_of_measurement": "°C",
        "state_class": "measurement",
        "suggested_display_precision": 0,
    })
}

fn entities(topics: &Topics, options: &[String], unit: &str) -> Vec<Entity> {
    let entity = |component, object, name, needs, fields| Entity {
        component,
        object,
        name,
        local_only: false,
        needs,
        fields,
    };
    let local = |component, object, name, needs, fields| Entity {
        component,
        object,
        name,
        local_only: true,
        needs,
        fields,
    };
    let options: Vec<String> = if options.is_empty() {
        vec!["No recipes".into()]
    } else {
        options.to_vec()
    };
    let progress = "value_json.get('cook_progress')";

    vec![
        // Sensors.
        entity(
            "sensor",
            "mode",
            "Mode",
            Needs::Oven,
            reading(topics, "phase", json!({})),
        ),
        entity(
            "sensor",
            "temperature",
            "Temperature",
            Needs::Oven,
            reading(topics, "temperature_c", celsius()),
        ),
        entity(
            "sensor",
            "target_temperature",
            "Target temperature",
            Needs::Oven,
            reading(topics, "target_temperature_c", celsius()),
        ),
        entity(
            "sensor",
            "wet_bulb_temperature",
            "Wet bulb temperature",
            Needs::Oven,
            reading(topics, "wet_bulb_temperature_c", celsius()),
        ),
        entity(
            "sensor",
            "probe_temperature",
            "Probe temperature",
            Needs::Oven,
            reading(topics, "probe_temperature_c", celsius()),
        ),
        entity(
            "sensor",
            "steam",
            "Steam",
            Needs::Oven,
            reading(
                topics,
                "steam_pct",
                json!({ "device_class": "humidity", "unit_of_measurement": "%", "state_class": "measurement" }),
            ),
        ),
        entity(
            "sensor",
            "timer_remaining",
            "Timer remaining",
            Needs::Oven,
            reading(
                topics,
                "timer_remaining_secs",
                json!({ "device_class": "duration", "unit_of_measurement": "s" }),
            ),
        ),
        entity(
            "sensor",
            "stage_ends_at",
            "Step ends at",
            Needs::Oven,
            reading(
                topics,
                "stage_ends_at",
                json!({ "device_class": "timestamp" }),
            ),
        ),
        entity(
            "sensor",
            "current_recipe",
            "Current recipe",
            Needs::Oven,
            reading(topics, "recipe_title", json!({})),
        ),
        entity(
            "sensor",
            "stage",
            "Step",
            Needs::Oven,
            json!({
                "state_topic": topics.state(),
                "value_template": format!(
                    "{{% set p = {progress} %}}{{{{ (p.current_step_index + 1) ~ ' of ' ~ p.step_count if p else none }}}}"
                ),
            }),
        ),
        entity(
            "sensor",
            "stage_description",
            "Step description",
            Needs::Oven,
            json!({
                "state_topic": topics.state(),
                "value_template": format!(
                    "{{% set p = {progress} %}}{{{{ p.current_stage_description if p else none }}}}"
                ),
            }),
        ),
        entity(
            "sensor",
            "selected_recipe",
            "Selected recipe",
            Needs::Server,
            json!({
                "state_topic": topics.selected(),
                "json_attributes_topic": topics.selected_attributes(),
            }),
        ),
        // Binary sensors.
        entity(
            "binary_sensor",
            "connected",
            "Connected",
            Needs::Server,
            on_off(topics, "value_json.connected", Some("connectivity")),
        ),
        entity(
            "binary_sensor",
            "cooking",
            "Cooking",
            Needs::Oven,
            on_off(topics, "value_json.mode != 'idle'", Some("running")),
        ),
        entity(
            "binary_sensor",
            "door",
            "Door",
            Needs::Oven,
            on_off(topics, "value_json.door_open", Some("door")),
        ),
        entity(
            "binary_sensor",
            "water_tank_empty",
            "Water tank empty",
            Needs::Oven,
            on_off(topics, "value_json.water_tank_empty", Some("problem")),
        ),
        entity(
            "binary_sensor",
            "next_stage_ready",
            "Next step ready",
            Needs::Oven,
            on_off(
                topics,
                &format!("{progress} and {progress}.next_stage_ready"),
                None,
            ),
        ),
        entity(
            "binary_sensor",
            "descale_required",
            "Descale required",
            Needs::Oven,
            on_off(
                topics,
                "value_json.boiler_descale_required",
                Some("problem"),
            ),
        ),
        // Choosing and starting a recipe.
        entity(
            "select",
            "recipe",
            "Recipe",
            Needs::Server,
            json!({
                "state_topic": topics.selected(),
                "command_topic": topics.command("select_recipe"),
                "options": options,
            }),
        ),
        entity(
            "button",
            "start",
            "Start",
            Needs::Oven,
            json!({ "command_topic": topics.command("start") }),
        ),
        entity(
            "button",
            "stop",
            "Stop",
            Needs::Oven,
            json!({ "command_topic": topics.command("stop") }),
        ),
        // A manual cook: one step, set up here and started with a button.
        // Editing it needs only the server; starting it, the oven too.
        entity(
            "number",
            "manual_temperature",
            "Manual temperature",
            Needs::Server,
            temperature_number(
                topics.manual(),
                "temperature_c",
                topics.command("manual/temperature"),
                (25, 250),
                unit,
            ),
        ),
        entity(
            "select",
            "manual_mode",
            "Manual mode",
            Needs::Server,
            manual_select(topics, "mode", &manual::MODES),
        ),
        entity(
            "number",
            "manual_steam",
            "Manual steam",
            Needs::Server,
            manual_number(
                topics,
                "steam_pct",
                "steam",
                (0, 100),
                json!({ "unit_of_measurement": "%" }),
            ),
        ),
        entity(
            "select",
            "manual_elements",
            "Manual heating elements",
            Needs::Server,
            manual_select(topics, "elements", &manual::ELEMENTS),
        ),
        entity(
            "select",
            "manual_preheat",
            "Manual preheat",
            Needs::Server,
            manual_select(topics, "preheat", &manual::PREHEATS),
        ),
        entity(
            "select",
            "manual_ends",
            "Manual ends",
            Needs::Server,
            manual_select(topics, "ends", &manual::ENDS),
        ),
        entity(
            "number",
            "manual_timer",
            "Manual timer",
            Needs::Server,
            manual_number(
                topics,
                "timer_mins",
                "timer",
                (1, 1440),
                json!({ "unit_of_measurement": "min" }),
            ),
        ),
        entity(
            "number",
            "manual_probe_target",
            "Manual probe target",
            Needs::Server,
            temperature_number(
                topics.manual(),
                "probe_target_c",
                topics.command("manual/probe_target"),
                (1, 100),
                unit,
            ),
        ),
        entity(
            "select",
            "manual_at_end",
            "Manual at end",
            Needs::Server,
            manual_select(topics, "at_end", &manual::AT_END),
        ),
        // What the button will start, or why it can't (`problem`).
        entity(
            "sensor",
            "manual_cook",
            "Manual cook",
            Needs::Server,
            json!({
                "state_topic": topics.manual(),
                "value_template": "{{ value_json.summary }}",
                "json_attributes_topic": topics.manual(),
                "json_attributes_template": "{{ {'problem': value_json.problem} | tojson }}",
            }),
        ),
        entity(
            "button",
            "start_manual",
            "Start manual cook",
            Needs::Oven,
            json!({ "command_topic": topics.command("start_manual") }),
        ),
        // Milestones.
        entity(
            "event",
            "cook",
            "Cook",
            Needs::Server,
            json!({ "state_topic": topics.event(), "event_types": EVENT_TYPES }),
        ),
        // Local mode only.
        local(
            "button",
            "next_stage",
            "Next step",
            Needs::WaitingStage,
            json!({ "command_topic": topics.command("next_stage") }),
        ),
        local(
            "light",
            "lamp",
            "Lamp",
            Needs::Oven,
            json!({
                "command_topic": topics.command("lamp"),
                "state_topic": topics.state(),
                "state_value_template": "{{ 'ON' if value_json.lamp_on else 'OFF' }}",
                "payload_on": "ON",
                "payload_off": "OFF",
            }),
        ),
        local(
            "number",
            "set_temperature",
            "Set temperature",
            Needs::Oven,
            temperature_number(
                topics.state(),
                "target_temperature_c",
                topics.command("temperature"),
                (25, 250),
                unit,
            ),
        ),
        local(
            "number",
            "set_timer",
            "Set timer",
            Needs::Oven,
            json!({
                "command_topic": topics.command("timer"),
                "command_template": "{{ (value * 60) | int }}",
                "state_topic": topics.state(),
                // No timer reads as 0, below min, which HA would log on every
                // state message; HA's payload_reset ("None") makes it unknown.
                "value_template": "{% set s = value_json.timer_total_secs %}{{ [(s / 60) | round(0) | int, 1] | max if s else 'None' }}",
                "min": 1,
                "max": 1440,
                "step": 1,
                "mode": "box",
                "unit_of_measurement": "min",
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/mqtt-discovery.json");

    fn topics() -> Topics {
        Topics {
            prefix: "anova/oven".into(),
            discovery: "homeassistant".into(),
        }
    }

    fn render(local: bool) -> Value {
        render_in(local, "C")
    }

    fn render_in(local: bool, unit: &str) -> Value {
        let configs = configs(&topics(), local, &["Bread".into(), "Toast".into()], unit);
        Value::Object(
            configs
                .into_iter()
                .map(|(topic, payload)| {
                    let v = if payload.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_str(&payload).unwrap()
                    };
                    (topic, v)
                })
                .collect(),
        )
    }

    /// The whole discovery set, local mode. To accept a deliberate change:
    /// `UPDATE_SNAPSHOTS=1 cargo test -p anova-oven-server discovery`.
    #[test]
    fn discovery_matches_the_snapshot() {
        let mut actual = render(true);
        // The version changes every release; it isn't part of the contract.
        for (_, v) in actual.as_object_mut().unwrap() {
            v["origin"]["sw_version"] = "x".into();
        }
        let pretty = serde_json::to_string_pretty(&actual).unwrap() + "\n";
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::write(SNAPSHOT, &pretty).unwrap();
        }
        let expected =
            std::fs::read_to_string(SNAPSHOT).expect("snapshot; run with UPDATE_SNAPSHOTS=1");
        assert_eq!(
            pretty, expected,
            "discovery changed; check unique_ids, then UPDATE_SNAPSHOTS=1"
        );
    }

    #[test]
    fn cloud_mode_removes_local_only_entities() {
        let cloud = render(false);
        let removed: Vec<&String> = cloud
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| v.is_null())
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            removed,
            [
                "homeassistant/button/anova_oven/next_stage/config",
                "homeassistant/light/anova_oven/lamp/config",
                "homeassistant/number/anova_oven/set_temperature/config",
                "homeassistant/number/anova_oven/set_timer/config",
            ]
        );
    }

    #[test]
    fn unique_ids_are_unique() {
        let all = render(true);
        let mut ids: Vec<&str> = all
            .as_object()
            .unwrap()
            .values()
            .map(|v| v["unique_id"].as_str().unwrap())
            .collect();
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    #[test]
    fn temperature_numbers_take_the_ovens_unit() {
        let all = render_in(true, "F");
        let manual = &all["homeassistant/number/anova_oven/manual_temperature/config"];
        assert_eq!(manual["unit_of_measurement"], "°F");
        assert_eq!(
            (manual["min"].clone(), manual["max"].clone()),
            (json!(77.0), json!(482.0))
        );
        let probe = &all["homeassistant/number/anova_oven/manual_probe_target/config"];
        assert_eq!(
            (probe["min"].clone(), probe["max"].clone()),
            (json!(34.0), json!(212.0))
        );
        assert_eq!(
            all["homeassistant/number/anova_oven/set_temperature/config"]["command_template"],
            "{{ (value - 32) * 5 / 9 }}"
        );
        // Everything else is the same in either unit.
        let celsius = render_in(true, "C");
        let differ: Vec<&String> = all
            .as_object()
            .unwrap()
            .iter()
            .filter(|(k, v)| celsius[k.as_str()] != **v)
            .map(|(k, _)| k)
            .collect();
        assert_eq!(
            differ,
            [
                "homeassistant/number/anova_oven/manual_probe_target/config",
                "homeassistant/number/anova_oven/manual_temperature/config",
                "homeassistant/number/anova_oven/set_temperature/config",
            ]
        );
    }

    #[test]
    fn an_empty_recipe_list_still_gives_the_select_an_option() {
        let (_, payload) = select_config(&topics(), &[]);
        let v: Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["options"], json!(["No recipes"]));
    }
}
