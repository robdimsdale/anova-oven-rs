//! Recipes as Home Assistant sees them: titles in a select, and the selected
//! recipe's details as a sensor's attributes. Recipe IDs never leave the
//! server.

use anova_oven_api::{Recipe, RecipeSource, Stage};

/// The select's options, sorted by title, each with the recipe ID it stands
/// for. Duplicate titles get a suffix (`Bread (2)`) so every option maps to
/// one recipe.
pub fn options(recipes: &[Recipe]) -> Vec<(String, String)> {
    let mut sorted: Vec<&Recipe> = recipes.iter().collect();
    sorted.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
    // Sorted, so recipes sharing a title are adjacent.
    let mut out: Vec<(String, String)> = Vec::with_capacity(sorted.len());
    let mut previous: Option<&str> = None;
    let mut nth = 0;
    for recipe in sorted {
        if previous == Some(recipe.title.as_str()) {
            nth += 1;
        } else {
            nth = 1;
            previous = Some(recipe.title.as_str());
        }
        let option = if nth == 1 {
            recipe.title.clone()
        } else {
            format!("{} ({nth})", recipe.title)
        };
        out.push((option, recipe.id.clone()));
    }
    out
}

/// The selected recipe sensor's attributes.
pub fn attributes(recipe: &Recipe, unit: &str) -> serde_json::Value {
    let total_timed_secs: u64 = recipe.stages.iter().filter_map(|s| s.duration_secs).sum();
    serde_json::json!({
        "title": recipe.title,
        "source": match recipe.source {
            RecipeSource::Own => "own",
            RecipeSource::Bookmarked => "bookmarked",
        },
        "stage_count": recipe.stages.len(),
        "total_timed_secs": total_timed_secs,
        "stages": recipe.stages.iter().map(|s| stage_attributes(s, unit)).collect::<Vec<_>>(),
    })
}

fn stage_attributes(stage: &Stage, unit: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": stage.kind,
        "title": stage.title,
        "temperature_c": stage.temperature_c,
        "bulb_mode": stage.temperature_bulbs_mode.as_deref().unwrap_or("dry"),
        "steam_pct": stage.steam_pct,
        "fan_speed": stage.fan_speed,
        "timer_secs": stage.duration_secs,
        "probe_target_c": stage.probe_target_c,
        "heating_elements": {
            "top": stage.heating_element_top.unwrap_or(true),
            "rear": stage.heating_element_rear.unwrap_or(true),
            "bottom": stage.heating_element_bottom.unwrap_or(true),
        },
        "waits_for_user": stage.user_action_required.unwrap_or(false),
        "summary": summary(stage, unit),
    })
}

/// One line per stage, e.g. `Cook · 220 °C dry · 30% steam · 25 min · waits
/// for you`, in the oven's display unit (`"C"` or `"F"`).
pub fn summary(stage: &Stage, unit: &str) -> String {
    let mut parts = Vec::new();
    parts.push(match stage.title.as_deref().filter(|t| !t.is_empty()) {
        Some(title) => title.to_string(),
        None => capitalise(&stage.kind),
    });
    let mode = stage.temperature_bulbs_mode.as_deref().unwrap_or("dry");
    parts.push(format!("{} {mode}", temperature(stage.temperature_c, unit)));
    if stage.steam_pct > 0.0 {
        parts.push(format!("{:.0}% steam", stage.steam_pct));
    }
    if let Some(secs) = stage.duration_secs {
        parts.push(duration(secs));
    } else if let Some(probe) = stage.probe_target_c {
        parts.push(format!("probe to {}", temperature(probe, unit)));
    }
    if stage.user_action_required == Some(true) {
        parts.push("waits for you".into());
    }
    parts.join(" · ")
}

fn temperature(celsius: f32, unit: &str) -> String {
    if unit.eq_ignore_ascii_case("F") {
        format!("{:.0} °F", celsius * 1.8 + 32.0)
    } else {
        format!("{celsius:.0} °C")
    }
}

/// `45 s`, `25 min`, `1 h 30 min`.
fn duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m, s) {
        (0, 0, s) => format!("{s} s"),
        (0, m, 0) => format!("{m} min"),
        (0, m, s) => format!("{m} min {s} s"),
        (h, 0, _) => format!("{h} h"),
        (h, m, _) => format!("{h} h {m} min"),
    }
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn stage(kind: &str) -> Stage {
        Stage {
            id: None,
            kind: kind.into(),
            temperature_c: 220.0,
            temperature_bulbs_mode: Some("dry".into()),
            duration_secs: None,
            timer_added: None,
            probe_added: None,
            probe_target_c: None,
            steam_pct: 0.0,
            fan_speed: 100,
            user_action_required: None,
            rack_position: None,
            heating_element_top: None,
            heating_element_rear: None,
            heating_element_bottom: None,
            vent_open: None,
            title: None,
        }
    }

    fn recipe(id: &str, title: &str) -> Recipe {
        Recipe {
            id: id.into(),
            title: title.into(),
            stage_count: 1,
            stages: vec![stage("cook")],
            source: RecipeSource::Own,
        }
    }

    #[test]
    fn options_are_sorted_and_duplicates_suffixed() {
        let recipes = [
            recipe("3", "Toast"),
            recipe("2", "Bread"),
            recipe("1", "Bread"),
        ];
        assert_eq!(
            options(&recipes),
            [
                ("Bread".to_string(), "1".to_string()),
                ("Bread (2)".to_string(), "2".to_string()),
                ("Toast".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    fn summaries() {
        let mut cook = stage("cook");
        cook.steam_pct = 30.0;
        cook.duration_secs = Some(25 * 60);
        cook.user_action_required = Some(true);
        assert_eq!(
            summary(&cook, "C"),
            "Cook · 220 °C dry · 30% steam · 25 min · waits for you"
        );

        let mut preheat = stage("preheat");
        preheat.title = Some("Heat up".into());
        assert_eq!(summary(&preheat, "F"), "Heat up · 428 °F dry");

        let mut probe = stage("cook");
        probe.temperature_bulbs_mode = Some("wet".into());
        probe.temperature_c = 60.0;
        probe.probe_target_c = Some(55.0);
        assert_eq!(summary(&probe, "C"), "Cook · 60 °C wet · probe to 55 °C");
    }

    #[test]
    fn durations() {
        assert_eq!(duration(45), "45 s");
        assert_eq!(duration(90), "1 min 30 s");
        assert_eq!(duration(5400), "1 h 30 min");
        assert_eq!(duration(7200), "2 h");
    }

    #[test]
    fn attributes_carry_the_stages() {
        let mut r = recipe("1", "Bread");
        r.stages[0].duration_secs = Some(600);
        let a = attributes(&r, "C");
        assert_eq!(a["stage_count"], 1);
        assert_eq!(a["total_timed_secs"], 600);
        assert_eq!(a["stages"][0]["summary"], "Cook · 220 °C dry · 10 min");
        assert_eq!(a["source"], "own");
    }
}
