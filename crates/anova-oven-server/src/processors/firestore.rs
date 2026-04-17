//! Firestore outbound processor.
//!
//! Owns Firebase session lifecycle, retry/timeout policy, and Firestore IO.

use std::time::Duration;

use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::firestore::{self, FirebaseSession, FirestoreError};
use crate::runtime::types::{FirestoreCommand, FirestoreEvent, FirestoreTaskError};

const DEFAULT_RECIPES_REFRESH_TIMEOUT_SECS: u64 = 15;
const DEFAULT_HISTORY_REFRESH_TIMEOUT_SECS: u64 = 15;

pub struct FirestoreProcessor {
    pub cmd_rx: mpsc::Receiver<FirestoreCommand>,
    pub evt_tx: mpsc::Sender<FirestoreEvent>,
    http: Client,
    session: FirebaseSession,
    current_cook_timeout: Duration,
    current_cook_resolution_timeout: Duration,
}

impl FirestoreProcessor {
    pub fn new(
        cmd_rx: mpsc::Receiver<FirestoreCommand>,
        evt_tx: mpsc::Sender<FirestoreEvent>,
        http: Client,
        session: FirebaseSession,
        current_cook_timeout: Duration,
        current_cook_resolution_timeout: Duration,
    ) -> Self {
        Self {
            cmd_rx,
            evt_tx,
            http,
            session,
            current_cook_timeout,
            current_cook_resolution_timeout,
        }
    }

    pub async fn run(mut self) {
        info!("[firestore] processor running");
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                FirestoreCommand::RefreshRecipes => {
                    let out = self.refresh_recipes().await;
                    let _ = self
                        .evt_tx
                        .send(FirestoreEvent::RecipesRefreshed(out))
                        .await;
                }
                FirestoreCommand::RefreshHistory { reason: _ } => {
                    let out = self.refresh_history().await;
                    let _ = self
                        .evt_tx
                        .send(FirestoreEvent::HistoryRefreshed(out))
                        .await;
                }
                FirestoreCommand::FetchCurrentCook { reason } => {
                    let out = self.fetch_current_cook().await;
                    let _ = self
                        .evt_tx
                        .send(FirestoreEvent::CurrentCookFetched {
                            reason,
                            result: out,
                        })
                        .await;
                }
                FirestoreCommand::PatchCookRecipeRef { cook_id, recipe_id } => {
                    let out = self
                        .set_cook_recipe_ref_with_retry(cook_id.clone(), recipe_id.clone())
                        .await;
                    let _ = self
                        .evt_tx
                        .send(FirestoreEvent::RecipeRefPatched {
                            cook_id,
                            recipe_id,
                            result: out,
                        })
                        .await;
                }
                FirestoreCommand::ResolveManualCookTitle { cook } => {
                    let out = self.resolve_manual_cook_title(cook).await;
                    let _ = self
                        .evt_tx
                        .send(FirestoreEvent::ManualCookTitleResolved {
                            cook_key: String::new(),
                            result: out,
                        })
                        .await;
                }
            }
        }
        warn!("[firestore] command channel closed");
    }

    async fn refresh_recipes(&mut self) -> Result<Vec<anova_oven_api::Recipe>, FirestoreTaskError> {
        match tokio::time::timeout(
            Duration::from_secs(DEFAULT_RECIPES_REFRESH_TIMEOUT_SECS),
            self.fetch_recipes_with_retry(),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(FirestoreTaskError::Timeout),
        }
    }

    async fn refresh_history(
        &mut self,
    ) -> Result<Vec<anova_oven_api::HistoryEntry>, FirestoreTaskError> {
        match tokio::time::timeout(
            Duration::from_secs(DEFAULT_HISTORY_REFRESH_TIMEOUT_SECS),
            self.fetch_history_with_retry(50),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(FirestoreTaskError::Timeout),
        }
    }

    async fn fetch_current_cook(
        &mut self,
    ) -> Result<Option<anova_oven_api::CurrentCook>, FirestoreTaskError> {
        match tokio::time::timeout(
            self.current_cook_timeout,
            self.fetch_current_cook_with_retry(),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(FirestoreTaskError::Timeout),
        }
    }

    async fn fetch_recipes_with_retry(
        &mut self,
    ) -> Result<Vec<anova_oven_api::Recipe>, FirestoreTaskError> {
        match firestore::fetch_recipes(&self.http, &self.session).await {
            Ok(v) => Ok(v),
            Err(err) => {
                let refreshed = self.maybe_refresh_session(err).await?;
                firestore::fetch_recipes(&self.http, &refreshed)
                    .await
                    .map_err(FirestoreTaskError::from)
            }
        }
    }

    async fn fetch_history_with_retry(
        &mut self,
        limit: usize,
    ) -> Result<Vec<anova_oven_api::HistoryEntry>, FirestoreTaskError> {
        let limit_u32 = u32::try_from(limit)
            .map_err(|_| FirestoreTaskError::Other("history limit conversion failed".into()))?;

        match firestore::fetch_history(&self.http, &self.session, limit_u32).await {
            Ok(v) => Ok(v),
            Err(err) => {
                let refreshed = self.maybe_refresh_session(err).await?;
                firestore::fetch_history(&self.http, &refreshed, limit_u32)
                    .await
                    .map_err(FirestoreTaskError::from)
            }
        }
    }

    async fn fetch_current_cook_with_retry(
        &mut self,
    ) -> Result<Option<anova_oven_api::CurrentCook>, FirestoreTaskError> {
        let mut cook = match firestore::fetch_current_cook(&self.http, &self.session).await {
            Ok(v) => v,
            Err(err) => {
                let refreshed = self.maybe_refresh_session(err).await?;
                firestore::fetch_current_cook(&self.http, &refreshed)
                    .await
                    .map_err(FirestoreTaskError::from)?
            }
        };

        if let Some(ref mut cook_value) = cook {
            if cook_value.recipe_title == "[manual]" {
                match tokio::time::timeout(
                    self.current_cook_resolution_timeout,
                    self.fetch_recipes_with_retry(),
                )
                .await
                {
                    Ok(Ok(recipes)) => {
                        if let Some((title, id)) = resolve_title_from_recipes(cook_value, &recipes)
                        {
                            cook_value.recipe_title = title;
                            if cook_value.recipe_id.is_none() {
                                cook_value.recipe_id = Some(id);
                            }
                        }
                    }
                    Ok(Err(_)) => {}
                    Err(_) => {}
                }
            }
        }

        Ok(cook)
    }

    async fn set_cook_recipe_ref_with_retry(
        &mut self,
        cook_id: String,
        recipe_id: String,
    ) -> Result<(), FirestoreTaskError> {
        const ATTEMPTS: u32 = 4;
        const INITIAL_DELAY_MS: u64 = 500;
        const RETRY_DELAY_MS: u64 = 750;

        tokio::time::sleep(Duration::from_millis(INITIAL_DELAY_MS)).await;

        for attempt in 1..=ATTEMPTS {
            let result = match firestore::patch_cook_recipe_ref(
                &self.http,
                &self.session,
                &cook_id,
                &recipe_id,
            )
            .await
            {
                Ok(()) => Ok(()),
                Err(err) => {
                    let refreshed = self.maybe_refresh_session(err).await?;
                    firestore::patch_cook_recipe_ref(&self.http, &refreshed, &cook_id, &recipe_id)
                        .await
                        .map_err(FirestoreTaskError::from)
                }
            };

            if result.is_ok() {
                return Ok(());
            }

            warn!(
                attempt,
                cook_id = %cook_id,
                recipe_id = %recipe_id,
                "[firestore] recipeRef patch attempt failed"
            );

            if attempt < ATTEMPTS {
                tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MS)).await;
            }
        }

        Err(FirestoreTaskError::Other(
            "recipeRef patch failed after retries".into(),
        ))
    }

    async fn resolve_manual_cook_title(
        &mut self,
        cook: anova_oven_api::CurrentCook,
    ) -> Result<Option<(String, String)>, FirestoreTaskError> {
        let recipes = self.fetch_recipes_with_retry().await?;
        Ok(resolve_title_from_recipes(&cook, &recipes))
    }

    async fn maybe_refresh_session(
        &mut self,
        err: FirestoreError,
    ) -> Result<FirebaseSession, FirestoreTaskError> {
        match err {
            FirestoreError::Unauthorized => {
                let mut refreshed = self.session.clone();
                firestore::refresh_session(&self.http, &mut refreshed)
                    .await
                    .map_err(|e| FirestoreTaskError::Other(e.to_string()))?;
                self.session = refreshed.clone();
                Ok(refreshed)
            }
            FirestoreError::Other(e) => Err(FirestoreTaskError::Other(e.to_string())),
        }
    }
}

fn approx_eq_f32(a: f32, b: f32, tolerance: f32) -> bool {
    (a - b).abs() <= tolerance
}

fn stage_semantically_matches(a: &anova_oven_api::Stage, b: &anova_oven_api::Stage) -> bool {
    a.kind == b.kind
        && approx_eq_f32(a.temperature_c, b.temperature_c, 1.0)
        && approx_eq_f32(a.steam_pct, b.steam_pct, 2.0)
        && a.duration_secs == b.duration_secs
        && match (a.probe_target_c, b.probe_target_c) {
            (Some(x), Some(y)) => approx_eq_f32(x, y, 1.0),
            (None, None) => true,
            _ => false,
        }
}

fn cook_matches_recipe(
    cook: &anova_oven_api::CurrentCook,
    recipe: &anova_oven_api::Recipe,
) -> bool {
    cook.stages.len() == recipe.stages.len()
        && cook
            .stages
            .iter()
            .zip(recipe.stages.iter())
            .all(|(c, r)| stage_semantically_matches(c, r))
}

fn resolve_title_from_recipes(
    cook: &anova_oven_api::CurrentCook,
    recipes: &[anova_oven_api::Recipe],
) -> Option<(String, String)> {
    if let Some(ref recipe_id) = cook.recipe_id {
        if let Some(recipe) = recipes.iter().find(|r| r.id == *recipe_id) {
            return Some((recipe.title.clone(), recipe.id.clone()));
        }
    }

    let mut matches = recipes.iter().filter(|r| cook_matches_recipe(cook, r));
    let first = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some((first.title.clone(), first.id.clone()))
}
