//! High-level document types for the `oven-recipes`, `favorite-oven-recipes`,
//! and `oven-cooks` collections.
//!
//! These are deserialized from the *unwrapped* JSON produced by
//! [`crate::firestore::Document::to_json`], not directly from the Firestore
//! REST response. See [`OvenRecipe::from_document`].

use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::firestore::Document;

/// A saved oven recipe document (`oven-recipes/{id}`).
///
/// Only the fields we care about for cook execution and listing are given
/// concrete typing; the remaining metadata is preserved as `extra` for
/// round-tripping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OvenRecipe {
    /// Firestore document ID (populated by [`OvenRecipe::from_document`];
    /// may be `None` when deserialized from an arbitrary document).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firestore_id: Option<String>,

    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub published: bool,
    #[serde(default)]
    pub created_timestamp: Option<String>,
    #[serde(default)]
    pub updated_timestamp: Option<String>,
    #[serde(default)]
    pub user_profile_ref: Option<String>,

    /// Recipe steps — a mix of `stepType: "stage"` (oven cook stages) and
    /// `stepType: "direction"` (text instructions). Kept as raw JSON so the
    /// caller can filter / forward to CMD_APO_START unchanged.
    #[serde(default)]
    pub steps: Vec<JsonValue>,

    #[serde(default)]
    pub ingredients: Vec<Ingredient>,

    /// Captures any fields not listed above (ratings, cover URLs, etc.).
    #[serde(flatten)]
    pub extra: serde_json::Map<String, JsonValue>,
}

impl OvenRecipe {
    /// Deserialize from a Firestore REST document, populating
    /// [`OvenRecipe::firestore_id`] from the document name.
    pub fn from_document(doc: &Document) -> Result<Self, serde_json::Error> {
        let json = doc.to_json();
        let mut recipe: OvenRecipe = serde_json::from_value(json)?;
        recipe.firestore_id = Some(doc.id().into());
        Ok(recipe)
    }

    /// Return only the steps where `stepType == "stage"` — the subset
    /// sent to the oven via `CMD_APO_START.stages`.
    pub fn cook_stages(&self) -> Vec<&JsonValue> {
        self.steps
            .iter()
            .filter(|s| {
                s.get("stepType")
                    .and_then(|v| v.as_str())
                    .map(|t| t == "stage")
                    .unwrap_or(false)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ingredient {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub quantity: f64,
    #[serde(default)]
    pub unit: String,
    #[serde(default)]
    pub quantity_display_type: String,
}

/// A recipe step. Not usually deserialized directly — `OvenRecipe::steps`
/// keeps the raw JSON for passthrough to the oven — but provided for callers
/// that want typed access.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    pub step_type: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, JsonValue>,
}

/// A bookmark (`users/{uid}/favorite-oven-recipes/{id}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FavoriteOvenRecipe {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firestore_id: Option<String>,
    /// Full document path of the referenced recipe
    /// (e.g. `projects/anova-app/databases/(default)/documents/oven-recipes/{id}`).
    #[serde(default)]
    pub recipe_ref: Option<String>,
    #[serde(default)]
    pub added_timestamp: Option<String>,
}

impl FavoriteOvenRecipe {
    pub fn from_document(doc: &Document) -> Result<Self, serde_json::Error> {
        let json = doc.to_json();
        let mut bm: FavoriteOvenRecipe = serde_json::from_value(json)?;
        bm.firestore_id = Some(doc.id().into());
        Ok(bm)
    }

    /// Extract just the `oven-recipes/{id}` portion of `recipe_ref`.
    ///
    /// Converts
    /// `projects/anova-app/databases/(default)/documents/oven-recipes/xxx`
    /// to `oven-recipes/xxx`.
    pub fn recipe_path(&self) -> Option<&str> {
        let r = self.recipe_ref.as_deref()?;
        r.split_once("/documents/").map(|(_, tail)| tail)
    }
}

/// A cook history entry (`users/{uid}/oven-cooks/{id}`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OvenCook {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub firestore_id: Option<String>,
    /// Cook ID as stored in the document (e.g. `ios-<uuid>`). Separate from
    /// the Firestore document ID.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub ended_timestamp: Option<String>,
    #[serde(default)]
    pub created_timestamp: Option<String>,
    /// Full path of the referenced recipe document, e.g.
    /// `projects/anova-app/databases/(default)/documents/oven-recipes/{id}`.
    #[serde(default)]
    pub recipe_ref: Option<String>,
    #[serde(default)]
    pub stages: Vec<JsonValue>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, JsonValue>,
}

impl OvenCook {
    pub fn from_document(doc: &Document) -> Result<Self, serde_json::Error> {
        let json = doc.to_json();
        let mut cook: OvenCook = serde_json::from_value(json)?;
        cook.firestore_id = Some(doc.id().into());
        Ok(cook)
    }

    /// Extract just the recipe document ID from `recipe_ref`.
    ///
    /// `projects/.../documents/oven-recipes/vRx7WOeVrgodAM0yePQ8`
    /// → `vRx7WOeVrgodAM0yePQ8`
    pub fn recipe_id(&self) -> Option<&str> {
        let r = self.recipe_ref.as_deref()?;
        r.rsplit('/').next().filter(|s| !s.is_empty())
    }
}
