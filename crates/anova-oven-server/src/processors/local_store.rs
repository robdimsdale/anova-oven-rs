//! Local store (local mode): recipes, history and the current cook kept as
//! files under `ANOVA_DATA_DIR`, in place of Firestore.
//!
//! - `recipes/*.json`: one [`Recipe`] per file. Written by
//!   `anova-oven-server export-recipes` and by the recipe API
//!   ([`RecipeFiles`]), and re-read on every refresh
//!   (`POST /api/update-recipes`). Editing them by hand works too.
//! - `active-cook.json`: a journal of the cook in flight, keyed by the
//!   oven's `cookId`. It holds what the oven doesn't know (recipe title and
//!   ID, start time) and when the oven last reported the cook. Written on
//!   `/start`, or on first sight of a cook started from the oven's panel,
//!   rewritten in place on every report of that cook, and removed when the
//!   cook goes to history. Never more than one small file.
//! - `history.jsonl`: one [`HistoryEntry`] per line.
//!
//! The oven's own `cook` block is the authority on the running cook: its
//! stages come from there, not from the recipe.
//!
//! Every oven report is reconciled against the journal
//! ([`LocalStore::oven_reported`]). When the journal names a cook the oven
//! isn't running, that cook has ended: it goes to history and the journal
//! moves on. That covers a cook ending while the server watches, a start
//! over a running cook, and a cook that ended while the server was down,
//! which is found on the first report after a restart and dated by its
//! last report.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anova_oven_api::{CurrentCook, HistoryEntry, Recipe};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::runtime::types::{OvenCook, StartedFrom, StoreCommand, StoreError, StoreEvent};

/// How many history entries `GET /history` returns, as the Firestore store.
const HISTORY_LIMIT: usize = 50;

const MANUAL_TITLE: &str = "[manual]";

/// `active-cook.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct ActiveCook {
    cook_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipe_id: Option<String>,
    recipe_title: String,
    /// ISO 8601. Also the cook's key in `CookProgressTask`, so it must not
    /// change during a cook.
    started_at: String,
    /// Cook stages, not counting preheats, as of the latest report.
    #[serde(default)]
    cook_stage_count: usize,
    /// When the oven last reported this cook (ISO 8601). `None` until it
    /// first does, so a `/start` the oven never acted on leaves no history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen_at: Option<String>,
}

pub struct LocalStore {
    dir: PathBuf,
    /// The cook in the previous oven report this process saw. A cook that
    /// was is known to have ended now; any other ended at its last report.
    watched: Option<String>,
}

impl LocalStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, watched: None }
    }

    pub async fn run(
        mut self,
        mut cmd_rx: mpsc::Receiver<StoreCommand>,
        evt_tx: mpsc::Sender<StoreEvent>,
    ) {
        info!(dir = %self.dir.display(), "[store] local store running");
        while let Some(cmd) = cmd_rx.recv().await {
            for evt in self.handle(cmd) {
                if evt_tx.send(evt).await.is_err() {
                    return;
                }
            }
        }
        warn!("[store] command channel closed");
    }

    fn handle(&mut self, cmd: StoreCommand) -> Vec<StoreEvent> {
        let now = jiff::Timestamp::now();
        let evt = match cmd {
            StoreCommand::RefreshRecipes => StoreEvent::RecipesRefreshed(self.recipes()),
            StoreCommand::RefreshHistory { reason: _ } => {
                StoreEvent::HistoryRefreshed(self.history())
            }
            StoreCommand::FetchCurrentCook { reason, oven_cook } => {
                StoreEvent::CurrentCookFetched {
                    reason,
                    result: self.current_cook(oven_cook.as_ref(), now),
                }
            }
            StoreCommand::PatchCookRecipeRef { cook_id, from } => {
                let result = match &from {
                    StartedFrom::Recipe(recipe_id) => self.cook_started(&cook_id, recipe_id, now),
                    StartedFrom::Manual {
                        title,
                        cook_stage_count,
                    } => self.journal_start(&cook_id, None, title.clone(), *cook_stage_count, now),
                };
                let recorded = matches!(result, Ok(true));
                let mut events = vec![StoreEvent::RecipeRefPatched {
                    cook_id,
                    recipe_id: from.recipe_id().map(String::from),
                    result: result.map(|_| ()),
                }];
                if recorded {
                    events.push(StoreEvent::HistoryRefreshed(self.history()));
                }
                return events;
            }
            StoreCommand::OvenReported { oven_cook } => {
                return match self.oven_reported(oven_cook.as_ref(), now) {
                    // Unprompted, so a cook found on startup shows at once.
                    Ok(true) => vec![StoreEvent::HistoryRefreshed(self.history())],
                    Ok(false) => Vec::new(),
                    Err(e) => {
                        warn!(error = %e, "[store] couldn't update the cook journal");
                        Vec::new()
                    }
                };
            }
            // Titles of manual cooks are only known to Anova's cloud.
            StoreCommand::ResolveManualCookTitle { cook: _ } => {
                StoreEvent::ManualCookTitleResolved {
                    cook_key: String::new(),
                    result: Ok(None),
                }
            }
        };
        vec![evt]
    }

    fn active_cook_path(&self) -> PathBuf {
        self.dir.join("active-cook.json")
    }

    fn history_path(&self) -> PathBuf {
        self.dir.join("history.jsonl")
    }

    fn recipes(&self) -> Result<Vec<Recipe>, StoreError> {
        RecipeFiles::new(&self.dir).list()
    }

    fn recipe(&self, id: &str) -> Option<Recipe> {
        self.recipes().ok()?.into_iter().find(|r| r.id == id)
    }

    /// `history.jsonl`, newest first.
    fn history(&self) -> Result<Vec<HistoryEntry>, StoreError> {
        let path = self.history_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_error(&path, e)),
        };
        let mut entries: Vec<HistoryEntry> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| match serde_json::from_str(line) {
                Ok(entry) => Some(entry),
                Err(e) => {
                    warn!(error = %e, "[store] skipping unreadable history line");
                    None
                }
            })
            .collect();
        entries.reverse();
        entries.truncate(HISTORY_LIMIT);
        Ok(entries)
    }

    /// `/start` sent `cook_id` for `recipe_id`: remember the recipe for when
    /// the oven reports that cook. A cook already in the journal is being
    /// replaced, so it goes to history first. Returns whether it did.
    fn cook_started(
        &mut self,
        cook_id: &str,
        recipe_id: &str,
        now: jiff::Timestamp,
    ) -> Result<bool, StoreError> {
        let (recipe_title, cook_stage_count) = match self.recipe(recipe_id) {
            Some(recipe) => {
                let count = cook_stage_count(&recipe.stages);
                (recipe.title, count)
            }
            None => {
                warn!(
                    recipe_id,
                    "[store] started recipe not found; recording as manual"
                );
                (MANUAL_TITLE.to_string(), 0)
            }
        };
        self.journal_start(
            cook_id,
            Some(recipe_id),
            recipe_title,
            cook_stage_count,
            now,
        )
    }

    /// Journal a cook the server just started, sending the one before it to
    /// history if it was a different cook. Returns whether it did.
    fn journal_start(
        &mut self,
        cook_id: &str,
        recipe_id: Option<&str>,
        recipe_title: String,
        cook_stage_count: usize,
        now: jiff::Timestamp,
    ) -> Result<bool, StoreError> {
        let recorded = match self.read_active_cook() {
            Some(previous) if previous.cook_id != cook_id => self.finish(previous, now)?,
            _ => false,
        };
        self.write_active_cook(&ActiveCook {
            cook_id: cook_id.to_string(),
            recipe_id: recipe_id.map(String::from),
            recipe_title,
            started_at: whole_seconds(now).to_string(),
            cook_stage_count,
            last_seen_at: None,
        })?;
        Ok(recorded)
    }

    /// Reconcile the journal with an oven report. Returns whether a cook went
    /// to history.
    fn oven_reported(
        &mut self,
        oven_cook: Option<&OvenCook>,
        now: jiff::Timestamp,
    ) -> Result<bool, StoreError> {
        let mut recorded = false;
        match (self.read_active_cook(), oven_cook) {
            (Some(mut active), Some(cook)) if active.cook_id == cook.cook_id => {
                active.last_seen_at = Some(whole_seconds(now).to_string());
                active.cook_stage_count = cook_stage_count(&cook.stages);
                self.write_active_cook(&active)?;
            }
            (active, cook) => {
                if let Some(active) = active {
                    recorded = self.finish(active, now)?;
                }
                if let Some(cook) = cook {
                    info!(cook_id = %cook.cook_id, "[store] recording a cook the server didn't start");
                    self.write_active_cook(&manual_cook(cook, now))?;
                }
            }
        }
        self.watched = oven_cook.map(|c| c.cook_id.clone());
        Ok(recorded)
    }

    /// The current cook: the oven's stages, plus the title and start time
    /// from the journal.
    fn current_cook(
        &mut self,
        oven_cook: Option<&OvenCook>,
        now: jiff::Timestamp,
    ) -> Result<Option<CurrentCook>, StoreError> {
        // No cook in the latest state: the state machine clears the current
        // cook itself on the cooking -> idle edge, and a start may still be
        // on its way to the oven, so don't contradict what it has.
        let Some(oven_cook) = oven_cook else {
            return Err(StoreError::Unsupported);
        };
        let active = match self.read_active_cook() {
            Some(active) if active.cook_id == oven_cook.cook_id => active,
            // Normally the report before this fetch already reconciled.
            _ => {
                self.oven_reported(Some(oven_cook), now)?;
                self.read_active_cook()
                    .ok_or_else(|| StoreError::Other("cook journal missing after write".into()))?
            }
        };
        Ok(Some(CurrentCook {
            recipe_title: active.recipe_title,
            recipe_id: active.recipe_id,
            started_at: active.started_at,
            cook_stage_count: cook_stage_count(&oven_cook.stages),
            total_stage_count: oven_cook.stages.len(),
            stages: oven_cook.stages.clone(),
        }))
    }

    /// The journal's cook has ended: append it to `history.jsonl` and clear
    /// the journal. It ended now if this process watched it until the last
    /// report, and at its last report otherwise. A cook the oven never
    /// reported is dropped. Returns whether a history entry was written.
    fn finish(&self, active: ActiveCook, now: jiff::Timestamp) -> Result<bool, StoreError> {
        let recorded = match &active.last_seen_at {
            Some(last_seen_at) => {
                let ended_at = if self.watched.as_deref() == Some(active.cook_id.as_str()) {
                    whole_seconds(now).to_string()
                } else {
                    last_seen_at.clone()
                };
                self.append_history(&HistoryEntry {
                    recipe_title: active.recipe_title,
                    ended_at,
                    stage_count: active.cook_stage_count,
                })?;
                true
            }
            None => {
                info!(cook_id = %active.cook_id, "[store] dropping a started cook the oven never reported");
                false
            }
        };
        let path = self.active_cook_path();
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_error(&path, e)),
        }
        Ok(recorded)
    }

    fn append_history(&self, entry: &HistoryEntry) -> Result<(), StoreError> {
        let path = self.history_path();
        let mut line =
            serde_json::to_string(entry).map_err(|e| StoreError::Other(e.to_string()))?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .map_err(|e| io_error(&path, e))?;
        info!(title = %entry.recipe_title, ended_at = %entry.ended_at, "[store] cook recorded in history");
        Ok(())
    }

    fn read_active_cook(&self) -> Option<ActiveCook> {
        let path = self.active_cook_path();
        let bytes = std::fs::read(&path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(active) => Some(active),
            Err(e) => {
                warn!(path = %path.display(), error = %e, "[store] ignoring unreadable active cook");
                None
            }
        }
    }

    fn write_active_cook(&self, active: &ActiveCook) -> Result<(), StoreError> {
        let json =
            serde_json::to_vec_pretty(active).map_err(|e| StoreError::Other(e.to_string()))?;
        write_atomically(&self.active_cook_path(), &json)
    }
}

/// A journal entry for a cook the server didn't start, seen now. Its start
/// is worked back from the oven's elapsed time.
fn manual_cook(cook: &OvenCook, now: jiff::Timestamp) -> ActiveCook {
    let started = now.as_second().saturating_sub(cook.seconds_elapsed as i64);
    ActiveCook {
        cook_id: cook.cook_id.clone(),
        recipe_id: None,
        recipe_title: MANUAL_TITLE.to_string(),
        started_at: jiff::Timestamp::from_second(started)
            .unwrap_or(now)
            .to_string(),
        cook_stage_count: cook_stage_count(&cook.stages),
        last_seen_at: Some(whole_seconds(now).to_string()),
    }
}

fn cook_stage_count(stages: &[anova_oven_api::Stage]) -> usize {
    stages.iter().filter(|s| s.kind != "preheat").count()
}

/// `recipes/*.json` under a data directory, for the recipe API. Separate
/// from [`LocalStore`] so HTTP handlers can read and write recipes without a
/// round trip through the state machine; the store only ever reads them.
/// Every write replaces a whole file atomically, so a concurrent read sees
/// the old recipe or the new one.
#[derive(Clone, Debug)]
pub struct RecipeFiles {
    dir: PathBuf,
}

/// Why a recipe write was refused.
#[derive(Debug)]
pub enum RecipeError {
    NotFound,
    Invalid(String),
    Store(StoreError),
}

impl From<StoreError> for RecipeError {
    fn from(e: StoreError) -> Self {
        RecipeError::Store(e)
    }
}

/// The fields a client sets on a recipe. Stages without an `id` get one.
#[derive(Debug, Deserialize)]
pub struct RecipeDraft {
    pub title: String,
    pub stages: Vec<anova_oven_api::Stage>,
}

/// A partial update: fields left out keep their value.
#[derive(Debug, Deserialize)]
pub struct RecipePatch {
    pub title: Option<String>,
    pub stages: Option<Vec<anova_oven_api::Stage>>,
}

impl RecipeFiles {
    /// The recipes under the data directory `data_dir`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("recipes"),
        }
    }

    /// Every recipe: the user's own first, then bookmarked ones, as the
    /// Firestore store lists them, each sorted by title. A file that doesn't parse is skipped
    /// with a warning rather than hiding the rest.
    pub fn list(&self) -> Result<Vec<Recipe>, StoreError> {
        let mut recipes: Vec<Recipe> = self.files()?.into_iter().map(|(_, r)| r).collect();
        recipes.sort_by(|a, b| {
            let bookmarked = |r: &Recipe| r.source == anova_oven_api::RecipeSource::Bookmarked;
            bookmarked(a)
                .cmp(&bookmarked(b))
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(recipes)
    }

    pub fn get(&self, id: &str) -> Result<Option<Recipe>, StoreError> {
        Ok(self.find(id)?.map(|(_, r)| r))
    }

    /// Save a new recipe of the user's own under a fresh ID.
    pub fn create(&self, draft: RecipeDraft) -> Result<Recipe, RecipeError> {
        let recipe = normalize(Recipe {
            id: uuid::Uuid::new_v4().simple().to_string(),
            title: draft.title,
            stage_count: 0,
            stages: draft.stages,
            source: anova_oven_api::RecipeSource::Own,
        })?;
        std::fs::create_dir_all(&self.dir).map_err(|e| io_error(&self.dir, e))?;
        self.write(&self.dir.join(recipe_file_name(&recipe.id)), &recipe)?;
        Ok(recipe)
    }

    /// Apply `patch` to recipe `id`, in the file it was read from.
    pub fn update(&self, id: &str, patch: RecipePatch) -> Result<Recipe, RecipeError> {
        let (path, mut recipe) = self.find(id)?.ok_or(RecipeError::NotFound)?;
        if let Some(title) = patch.title {
            recipe.title = title;
        }
        if let Some(stages) = patch.stages {
            recipe.stages = stages;
        }
        let recipe = normalize(recipe)?;
        self.write(&path, &recipe)?;
        Ok(recipe)
    }

    pub fn delete(&self, id: &str) -> Result<(), RecipeError> {
        let (path, _) = self.find(id)?.ok_or(RecipeError::NotFound)?;
        std::fs::remove_file(&path).map_err(|e| io_error(&path, e))?;
        Ok(())
    }

    /// Recipe `id` and its file. Looked up by content rather than file name,
    /// since a file added by hand may be named anything.
    fn find(&self, id: &str) -> Result<Option<(PathBuf, Recipe)>, StoreError> {
        Ok(self.files()?.into_iter().find(|(_, r)| r.id == id))
    }

    fn files(&self) -> Result<Vec<(PathBuf, Recipe)>, StoreError> {
        let dir = &self.dir;
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                warn!(dir = %dir.display(), "[store] no recipes directory; run `export-recipes` or add recipes");
                return Ok(Vec::new());
            }
            Err(e) => return Err(io_error(dir, e)),
        };
        let mut recipes = Vec::new();
        for entry in entries {
            let path = entry.map_err(|e| io_error(dir, e))?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let parsed = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<Recipe>(&bytes).map_err(|e| e.to_string())
                });
            match parsed {
                Ok(recipe) => recipes.push((path, recipe)),
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "[store] skipping unreadable recipe")
                }
            }
        }
        Ok(recipes)
    }

    fn write(&self, path: &Path, recipe: &Recipe) -> Result<(), StoreError> {
        let json =
            serde_json::to_vec_pretty(recipe).map_err(|e| StoreError::Other(e.to_string()))?;
        write_atomically(path, &json)
    }
}

/// Check a recipe the oven could run, give new stages IDs, and recount its
/// cook stages.
pub fn normalize(mut recipe: Recipe) -> Result<Recipe, RecipeError> {
    let invalid = |msg: String| Err(RecipeError::Invalid(msg));
    recipe.title = recipe.title.trim().to_string();
    if recipe.title.is_empty() {
        return invalid("title can't be empty".into());
    }
    if recipe.stages.is_empty() {
        return invalid("a recipe needs at least one stage".into());
    }
    for (i, stage) in recipe.stages.iter_mut().enumerate() {
        let n = i + 1;
        if stage.kind != "preheat" && stage.kind != "cook" {
            return invalid(format!("stage {n}: kind must be preheat or cook"));
        }
        let wet = stage.temperature_bulbs_mode.as_deref() == Some("wet");
        // Sous vide tops out at 100 °C on the original Precision Oven.
        let (range, what) = if wet {
            (
                25.0..=100.0,
                "sous vide temperature must be within 25–100 °C",
            )
        } else {
            (25.0..=250.0, "temperature must be within 25–250 °C")
        };
        if !range.contains(&stage.temperature_c) {
            return invalid(format!("stage {n}: {what}"));
        }
        if !(0.0..=100.0).contains(&stage.steam_pct) {
            return invalid(format!("stage {n}: steam must be within 0–100 %"));
        }
        if stage.fan_speed > 100 {
            return invalid(format!("stage {n}: fan must be within 0–100 %"));
        }
        if stage.rack_position.is_some_and(|r| !(1..=5).contains(&r)) {
            return invalid(format!("stage {n}: rack must be within 1–5"));
        }
        if stage.timer_added == Some(true) && !stage.duration_secs.is_some_and(|d| d > 0) {
            return invalid(format!("stage {n}: a timer needs a duration"));
        }
        if stage.probe_added == Some(true) && stage.probe_target_c.is_none() {
            return invalid(format!("stage {n}: a probe stage needs a target"));
        }
        if stage
            .probe_target_c
            .is_some_and(|t| !(1.0..=100.0).contains(&t))
        {
            return invalid(format!(
                "stage {n}: the probe target must be within 1–100 °C"
            ));
        }
        if stage.timer_added == Some(true) && stage.probe_added == Some(true) {
            return invalid(format!(
                "stage {n}: a stage takes a timer or a probe, not both"
            ));
        }
        if stage.kind == "preheat"
            && (stage.timer_added == Some(true) || stage.probe_added == Some(true))
        {
            return invalid(format!(
                "stage {n}: a preheat ends at temperature, without a timer or probe"
            ));
        }
        let elements = [
            stage.heating_element_top,
            stage.heating_element_rear,
            stage.heating_element_bottom,
        ];
        // Any one or two, never all three or none (Anova's oven-commands docs).
        let on = elements.iter().filter(|e| **e == Some(true)).count();
        if !(1..=2).contains(&on) {
            return invalid(format!("stage {n}: use one or two heating elements"));
        }
        // The firmware won't run the rear (convection) element, or steam,
        // with the fan off.
        stage.normalize_fan_speed();
        if stage.title.as_deref().is_some_and(|t| t.trim().is_empty()) {
            stage.title = None;
        }
        if stage.id.as_deref().is_none_or(str::is_empty) {
            stage.id = Some(format!("ios-{}", uuid::Uuid::new_v4()));
        }
    }
    recipe.stage_count = cook_stage_count(&recipe.stages);
    Ok(recipe)
}

/// Write each recipe to `<dir>/recipes/<id>.json`, replacing a file with the
/// same ID. Returns the recipes directory.
pub fn write_recipes(dir: &Path, recipes: &[Recipe]) -> Result<PathBuf, StoreError> {
    let recipes_dir = dir.join("recipes");
    std::fs::create_dir_all(&recipes_dir).map_err(|e| io_error(&recipes_dir, e))?;
    for recipe in recipes {
        let json =
            serde_json::to_vec_pretty(recipe).map_err(|e| StoreError::Other(e.to_string()))?;
        write_atomically(&recipes_dir.join(recipe_file_name(&recipe.id)), &json)?;
    }
    Ok(recipes_dir)
}

/// A file name for a recipe ID: anything but ASCII letters, digits, `-` and
/// `_` becomes `_`.
fn recipe_file_name(id: &str) -> String {
    let stem: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{stem}.json")
}

/// Write to a temporary file and rename it over `path`, so a crash never
/// leaves a half-written file.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| io_error(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| io_error(path, e))
}

fn whole_seconds(ts: jiff::Timestamp) -> jiff::Timestamp {
    jiff::Timestamp::from_second(ts.as_second()).unwrap_or(ts)
}

fn io_error(path: &Path, e: std::io::Error) -> StoreError {
    StoreError::Other(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests;
