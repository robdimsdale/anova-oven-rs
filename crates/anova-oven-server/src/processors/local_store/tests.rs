use std::path::PathBuf;

use anova_oven_api::{Recipe, Stage};

use super::{
    recipe_file_name, write_recipes, LocalStore, RecipeDraft, RecipeError, RecipeFiles,
    RecipePatch, MANUAL_TITLE,
};
use crate::runtime::types::{OvenCook, StoreError};

/// A data directory removed when the test ends.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("anova-local-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn stage(kind: &str) -> Stage {
    Stage {
        id: Some(format!("{kind}-1")),
        kind: kind.into(),
        temperature_c: 200.0,
        temperature_bulbs_mode: Some("dry".into()),
        duration_secs: (kind == "cook").then_some(600),
        timer_added: None,
        probe_added: None,
        probe_target_c: None,
        steam_pct: 0.0,
        fan_speed: 100,
        user_action_required: Some(false),
        rack_position: Some(3),
        heating_element_top: Some(true),
        heating_element_rear: Some(true),
        heating_element_bottom: Some(false),
        vent_open: Some(false),
        title: None,
    }
}

fn recipe(id: &str, title: &str) -> Recipe {
    Recipe {
        id: id.into(),
        title: title.into(),
        stage_count: 1,
        stages: vec![stage("preheat"), stage("cook")],
        source: Default::default(),
    }
}

fn oven_cook(cook_id: &str, seconds_elapsed: u64) -> OvenCook {
    OvenCook {
        cook_id: cook_id.into(),
        stages: vec![stage("preheat"), stage("cook")],
        raw_stages: Vec::new(),
        seconds_elapsed,
    }
}

fn ts(s: &str) -> jiff::Timestamp {
    s.parse().unwrap()
}

fn journal(dir: &TempDir) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(dir.0.join("active-cook.json")).unwrap()).unwrap()
}

fn store_with_bread(dir: &TempDir) -> LocalStore {
    write_recipes(&dir.0, &[recipe("r1", "Bread")]).unwrap();
    LocalStore::new(dir.0.clone())
}

#[test]
fn exported_recipes_read_back_own_first_then_by_title() {
    let dir = TempDir::new();
    let mut bagels = recipe("c3", "Bagels");
    bagels.source = anova_oven_api::RecipeSource::Bookmarked;
    write_recipes(
        &dir.0,
        &[recipe("b/2", "Toast"), bagels, recipe("a1", "Bread")],
    )
    .unwrap();
    // Not a recipe: skipped, and doesn't hide the others.
    std::fs::write(dir.0.join("recipes/broken.json"), "{").unwrap();

    let store = LocalStore::new(dir.0.clone());
    let titles: Vec<String> = store
        .recipes()
        .unwrap()
        .into_iter()
        .map(|r| r.title)
        .collect();
    assert_eq!(titles, ["Bread", "Toast", "Bagels"]);
    assert!(dir.0.join("recipes/b_2.json").exists());
}

#[test]
fn no_recipes_directory_means_no_recipes() {
    let dir = TempDir::new();
    assert!(LocalStore::new(dir.0.clone()).recipes().unwrap().is_empty());
}

#[test]
fn started_cook_keeps_its_title_and_start_time() {
    let dir = TempDir::new();
    let mut store = store_with_bread(&dir);
    store
        .cook_started("ios-cook", "r1", ts("2026-10-15T18:00:00.5Z"))
        .unwrap();

    // Later, and again after a restart (a new store over the same files).
    for mut store in [store, LocalStore::new(dir.0.clone())] {
        let cook = store
            .current_cook(
                Some(&oven_cook("ios-cook", 300)),
                ts("2026-10-15T18:05:00Z"),
            )
            .unwrap()
            .unwrap();
        assert_eq!(cook.recipe_title, "Bread");
        assert_eq!(cook.recipe_id.as_deref(), Some("r1"));
        assert_eq!(cook.started_at, "2026-10-15T18:00:00Z");
        assert_eq!(cook.total_stage_count, 2);
        assert_eq!(cook.cook_stage_count, 1);
    }
}

#[test]
fn a_cook_from_the_panel_is_manual_with_a_stable_start() {
    let dir = TempDir::new();
    let mut store = LocalStore::new(dir.0.clone());
    let first = store
        .current_cook(
            Some(&oven_cook("oven-cook", 60)),
            ts("2026-10-15T18:01:00Z"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(first.recipe_title, MANUAL_TITLE);
    assert_eq!(first.recipe_id, None);
    assert_eq!(first.started_at, "2026-10-15T18:00:00Z");

    // The elapsed time and the clock don't move in step; the start time must
    // stay put, since it keys the cook.
    let again = store
        .current_cook(
            Some(&oven_cook("oven-cook", 62)),
            ts("2026-10-15T18:01:03Z"),
        )
        .unwrap()
        .unwrap();
    assert_eq!(again.started_at, first.started_at);
}

#[test]
fn no_oven_cook_leaves_the_state_machine_alone() {
    let dir = TempDir::new();
    let result = LocalStore::new(dir.0.clone()).current_cook(None, jiff::Timestamp::now());
    assert!(matches!(result, Err(StoreError::Unsupported)));
}

#[test]
fn each_report_rewrites_the_journal_in_place() {
    let dir = TempDir::new();
    let mut store = store_with_bread(&dir);
    store
        .cook_started("ios-1", "r1", ts("2026-10-15T17:00:00Z"))
        .unwrap();
    assert!(journal(&dir).get("last_seen_at").is_none());

    for t in ["2026-10-15T17:00:10Z", "2026-10-15T17:00:20Z"] {
        assert!(!store
            .oven_reported(Some(&oven_cook("ios-1", 10)), ts(t))
            .unwrap());
    }
    assert_eq!(journal(&dir)["last_seen_at"], "2026-10-15T17:00:20Z");
    assert_eq!(journal(&dir)["recipe_title"], "Bread");
    // Just recipes/ and the journal: nothing accumulates.
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
}

#[test]
fn a_cook_watched_to_the_end_ends_when_the_oven_goes_idle() {
    let dir = TempDir::new();
    let mut store = store_with_bread(&dir);
    store
        .cook_started("ios-1", "r1", ts("2026-10-15T17:00:00Z"))
        .unwrap();
    store
        .oven_reported(Some(&oven_cook("ios-1", 10)), ts("2026-10-15T17:00:10Z"))
        .unwrap();
    assert!(store
        .oven_reported(None, ts("2026-10-15T17:30:05Z"))
        .unwrap());

    assert!(!dir.0.join("active-cook.json").exists());
    let history = store.history().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].recipe_title, "Bread");
    assert_eq!(history[0].ended_at, "2026-10-15T17:30:05Z");
    assert_eq!(history[0].stage_count, 1);
}

#[test]
fn a_cook_that_ended_while_the_server_was_down_is_found_on_restart() {
    let dir = TempDir::new();
    let mut before = store_with_bread(&dir);
    before
        .cook_started("ios-1", "r1", ts("2026-10-15T17:00:00Z"))
        .unwrap();
    before
        .oven_reported(Some(&oven_cook("ios-1", 10)), ts("2026-10-15T17:20:00Z"))
        .unwrap();
    drop(before);

    // Back up after the cook ended: the first report finds the oven idle.
    let mut after = LocalStore::new(dir.0.clone());
    assert!(after
        .oven_reported(None, ts("2026-10-15T19:00:00Z"))
        .unwrap());
    let history = after.history().unwrap();
    assert_eq!(history[0].recipe_title, "Bread");
    assert_eq!(history[0].ended_at, "2026-10-15T17:20:00Z");
}

#[test]
fn a_different_cook_after_a_restart_ends_the_journalled_one() {
    let dir = TempDir::new();
    let mut before = LocalStore::new(dir.0.clone());
    before
        .oven_reported(Some(&oven_cook("panel-1", 0)), ts("2026-10-15T17:00:00Z"))
        .unwrap();
    drop(before);

    let mut after = LocalStore::new(dir.0.clone());
    assert!(after
        .oven_reported(Some(&oven_cook("panel-2", 30)), ts("2026-10-15T19:00:30Z"))
        .unwrap());
    assert_eq!(after.history().unwrap()[0].ended_at, "2026-10-15T17:00:00Z");
    assert_eq!(journal(&dir)["cook_id"], "panel-2");
    assert_eq!(journal(&dir)["started_at"], "2026-10-15T19:00:00Z");
}

#[test]
fn a_start_the_oven_never_acted_on_leaves_no_history() {
    let dir = TempDir::new();
    let mut store = store_with_bread(&dir);
    store
        .cook_started("ios-1", "r1", ts("2026-10-15T17:00:00Z"))
        .unwrap();
    assert!(!store
        .oven_reported(None, ts("2026-10-15T17:00:30Z"))
        .unwrap());
    assert!(store.history().unwrap().is_empty());
    assert!(!dir.0.join("active-cook.json").exists());
}

#[test]
fn a_start_over_a_running_cook_ends_it() {
    let dir = TempDir::new();
    let mut store = store_with_bread(&dir);
    store
        .oven_reported(Some(&oven_cook("panel-1", 0)), ts("2026-10-15T17:00:00Z"))
        .unwrap();
    assert!(store
        .cook_started("ios-2", "r1", ts("2026-10-15T17:10:00Z"))
        .unwrap());
    let history = store.history().unwrap();
    assert_eq!(history[0].recipe_title, MANUAL_TITLE);
    assert_eq!(history[0].ended_at, "2026-10-15T17:10:00Z");
    assert_eq!(journal(&dir)["cook_id"], "ios-2");
}

#[test]
fn history_is_newest_first() {
    let dir = TempDir::new();
    let mut store = LocalStore::new(dir.0.clone());
    for (id, at) in [("a", "2026-10-15T17:00:00Z"), ("b", "2026-10-15T18:00:00Z")] {
        store
            .oven_reported(Some(&oven_cook(id, 0)), ts(at))
            .unwrap();
        store.oven_reported(None, ts(at)).unwrap();
    }
    let ended: Vec<String> = store
        .history()
        .unwrap()
        .into_iter()
        .map(|h| h.ended_at)
        .collect();
    assert_eq!(ended, ["2026-10-15T18:00:00Z", "2026-10-15T17:00:00Z"]);
}

#[test]
fn recipe_ids_become_safe_file_names() {
    assert_eq!(recipe_file_name("abc-DEF_1"), "abc-DEF_1.json");
    assert_eq!(recipe_file_name("../x y"), "___x_y.json");
}

fn draft(title: &str, stages: Vec<Stage>) -> RecipeDraft {
    RecipeDraft {
        title: title.into(),
        stages,
    }
}

#[test]
fn a_created_recipe_reads_back_with_ids_and_a_count() {
    let dir = TempDir::new();
    let files = RecipeFiles::new(&dir.0);
    let mut cook = stage("cook");
    cook.id = None;
    let created = files
        .create(draft("  Bagels ", vec![stage("preheat"), cook]))
        .unwrap();

    assert_eq!(created.title, "Bagels");
    assert_eq!(created.stage_count, 1);
    assert_eq!(created.source, anova_oven_api::RecipeSource::Own);
    let new_id = created.stages[1].id.as_deref().unwrap();
    assert!(new_id.starts_with("ios-"), "{new_id}");
    // Existing stage IDs are kept.
    assert_eq!(created.stages[0].id.as_deref(), Some("preheat-1"));

    let read = files.get(&created.id).unwrap().unwrap();
    assert_eq!(read.title, "Bagels");
    // The store sees it too.
    assert_eq!(LocalStore::new(dir.0.clone()).recipes().unwrap().len(), 1);
}

#[test]
fn a_patch_changes_only_what_it_names() {
    let dir = TempDir::new();
    write_recipes(&dir.0, &[recipe("r1", "Bread")]).unwrap();
    let files = RecipeFiles::new(&dir.0);

    let renamed = files
        .update(
            "r1",
            RecipePatch {
                title: Some("Rye".into()),
                stages: None,
            },
        )
        .unwrap();
    assert_eq!(renamed.title, "Rye");
    assert_eq!(renamed.stages.len(), 2);

    let restaged = files
        .update(
            "r1",
            RecipePatch {
                title: None,
                stages: Some(vec![stage("cook"), stage("cook")]),
            },
        )
        .unwrap();
    assert_eq!(restaged.title, "Rye");
    assert_eq!(restaged.stage_count, 2);
    assert_eq!(files.get("r1").unwrap().unwrap().stage_count, 2);
}

#[test]
fn a_recipe_in_a_hand_named_file_is_updated_and_deleted_in_place() {
    let dir = TempDir::new();
    std::fs::create_dir_all(dir.0.join("recipes")).unwrap();
    let path = dir.0.join("recipes/my bread.json");
    std::fs::write(&path, serde_json::to_vec(&recipe("r1", "Bread")).unwrap()).unwrap();
    let files = RecipeFiles::new(&dir.0);

    files
        .update(
            "r1",
            RecipePatch {
                title: Some("Rye".into()),
                stages: None,
            },
        )
        .unwrap();
    let names: Vec<_> = std::fs::read_dir(dir.0.join("recipes"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["my bread.json"]);

    files.delete("r1").unwrap();
    assert!(!path.exists());
    assert!(matches!(files.delete("r1"), Err(RecipeError::NotFound)));
}

#[test]
fn recipes_the_oven_cant_run_are_refused() {
    let dir = TempDir::new();
    let files = RecipeFiles::new(&dir.0);
    let invalid = |d: RecipeDraft| matches!(files.create(d), Err(RecipeError::Invalid(_)));

    assert!(invalid(draft(" ", vec![stage("cook")])));
    assert!(invalid(draft("Empty", vec![])));
    let mut hot = stage("cook");
    hot.temperature_c = 300.0;
    assert!(invalid(draft("Hot", vec![hot])));
    let mut odd = stage("cook");
    odd.kind = "broil".into();
    assert!(invalid(draft("Odd", vec![odd])));
    let mut timer = stage("cook");
    timer.timer_added = Some(true);
    timer.duration_secs = None;
    assert!(invalid(draft("Timer", vec![timer])));
    let mut sous_vide = stage("cook");
    sous_vide.temperature_bulbs_mode = Some("wet".into());
    sous_vide.temperature_c = 120.0;
    assert!(invalid(draft("Sous vide", vec![sous_vide])));
    let mut both = stage("cook");
    both.timer_added = Some(true);
    both.probe_added = Some(true);
    both.probe_target_c = Some(60.0);
    assert!(invalid(draft("Both", vec![both])));
    let mut timed_preheat = stage("preheat");
    timed_preheat.timer_added = Some(true);
    timed_preheat.duration_secs = Some(60);
    assert!(invalid(draft("Timed preheat", vec![timed_preheat])));
    let mut cold = stage("cook");
    cold.heating_element_top = Some(false);
    cold.heating_element_rear = Some(false);
    cold.heating_element_bottom = Some(false);
    assert!(invalid(draft("Cold", vec![cold])));
    let mut all_three = stage("cook");
    all_three.heating_element_bottom = Some(true);
    assert!(invalid(draft("All three", vec![all_three])));
    let mut hot_probe = stage("cook");
    hot_probe.probe_added = Some(true);
    hot_probe.probe_target_c = Some(120.0);
    assert!(invalid(draft("Hot probe", vec![hot_probe])));
    // Nothing was written.
    assert!(files.list().unwrap().is_empty());
}

#[test]
fn the_fan_goes_on_for_the_rear_element_or_steam() {
    let dir = TempDir::new();
    let files = RecipeFiles::new(&dir.0);
    let mut rear = stage("cook");
    rear.fan_speed = 0;
    let mut top_only = stage("cook");
    top_only.heating_element_rear = Some(false);
    top_only.fan_speed = 0;
    let saved = files.create(draft("Fans", vec![rear, top_only])).unwrap();
    assert_eq!(saved.stages[0].fan_speed, 100);
    assert_eq!(saved.stages[1].fan_speed, 0);
}

#[test]
fn a_manual_cook_is_journalled_under_its_title_and_goes_to_history() {
    let dir = TempDir::new();
    let mut store = LocalStore::new(dir.0.clone());
    store
        .journal_start(
            "ios-m",
            None,
            "Quick roast".into(),
            1,
            ts("2026-10-15T18:00:00Z"),
        )
        .unwrap();
    let cook = store
        .current_cook(Some(&oven_cook("ios-m", 60)), ts("2026-10-15T18:01:00Z"))
        .unwrap()
        .unwrap();
    assert_eq!(cook.recipe_title, "Quick roast");
    assert_eq!(cook.recipe_id, None);

    store
        .oven_reported(Some(&oven_cook("ios-m", 60)), ts("2026-10-15T18:01:00Z"))
        .unwrap();
    // The oven goes idle: it goes to history under its title.
    assert!(store
        .oven_reported(None, ts("2026-10-15T18:10:00Z"))
        .unwrap());
    assert_eq!(store.history().unwrap()[0].recipe_title, "Quick roast");
}
