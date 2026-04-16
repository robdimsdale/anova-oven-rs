//! Firebase Auth + Firestore client, absorbed from `anova-oven-firestore`.
//!
//! Handles sign-in, token refresh, structured queries, and document parsing.
//! Converts Firestore documents into [`anova_oven_api`] types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};
use tracing::{debug, warn};

// ─── Constants ────────────────────────────────────────────────────────────────

pub const ANOVA_PROJECT_ID: &str = "anova-app";
const ANOVA_OVEN_API_KEY: &str = "AIzaSyCGJwHXUhkNBdPkH3OAkjc9-3xMMjvanfU";

// ─── Firebase Auth ─────────────────────────────────────────────────────────────

fn sign_in_url() -> String {
    format!(
        "https://identitytoolkit.googleapis.com/v1/accounts:signInWithPassword?key={ANOVA_OVEN_API_KEY}"
    )
}

fn refresh_token_url() -> String {
    format!("https://securetoken.googleapis.com/v1/token?key={ANOVA_OVEN_API_KEY}")
}

fn build_refresh_form(refresh_token: &str) -> String {
    let mut encoded = String::with_capacity(refresh_token.len() + 48);
    encoded.push_str("grant_type=refresh_token&refresh_token=");
    for b in refresh_token.as_bytes() {
        let c = *b;
        let safe = c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'~');
        if safe {
            encoded.push(c as char);
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{:02X}", c);
        }
    }
    encoded
}

#[derive(Serialize)]
struct SignInRequest<'a> {
    email: &'a str,
    password: &'a str,
    #[serde(rename = "returnSecureToken")]
    return_secure_token: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignInResponse {
    id_token: String,
    local_id: String,
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct RefreshTokenResponse {
    id_token: String,
    refresh_token: String,
    user_id: String,
}

/// Authenticated Firebase session stored in memory by the server.
#[derive(Debug, Clone)]
pub struct FirebaseSession {
    pub id_token: String,
    pub uid: String,
    pub refresh_token: String,
}

/// Error returned by Firestore fetch operations.
#[derive(Debug)]
pub enum FirestoreError {
    /// HTTP 401 / UNAUTHENTICATED — the Firebase ID token has expired.
    /// The caller should call [`refresh_session`] and retry.
    Unauthorized,
    /// Any other error.
    Other(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for FirestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FirestoreError::Unauthorized => write!(f, "Unauthorized (Firebase token expired)"),
            FirestoreError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FirestoreError {}

/// Sign in with email + password, returning a [`FirebaseSession`].
pub async fn sign_in(
    client: &reqwest::Client,
    email: &str,
    password: &str,
) -> Result<FirebaseSession, Box<dyn std::error::Error + Send + Sync>> {
    let resp = client
        .post(sign_in_url())
        .json(&SignInRequest {
            email,
            password,
            return_secure_token: true,
        })
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Firebase sign-in failed ({status}): {body}").into());
    }
    let parsed: SignInResponse = resp.json().await?;
    Ok(FirebaseSession {
        id_token: parsed.id_token,
        uid: parsed.local_id,
        refresh_token: parsed.refresh_token,
    })
}

/// Exchange a refresh token for a fresh ID token, updating the session in place.
pub async fn refresh_session(
    client: &reqwest::Client,
    session: &mut FirebaseSession,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let body = build_refresh_form(&session.refresh_token);
    let resp = client
        .post(refresh_token_url())
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Firebase token refresh failed ({status}): {body}").into());
    }
    let parsed: RefreshTokenResponse = resp.json().await?;
    session.id_token = parsed.id_token;
    session.refresh_token = parsed.refresh_token;
    session.uid = parsed.user_id;
    Ok(())
}

// ─── Firestore REST types ──────────────────────────────────────────────────────

fn run_query_url(parent_path: &str) -> String {
    if parent_path.is_empty() {
        format!(
            "https://firestore.googleapis.com/v1/projects/{ANOVA_PROJECT_ID}/databases/(default)/documents:runQuery"
        )
    } else {
        format!(
            "https://firestore.googleapis.com/v1/projects/{ANOVA_PROJECT_ID}/databases/(default)/documents/{parent_path}:runQuery"
        )
    }
}

fn get_document_url(document_path: &str) -> String {
    format!(
        "https://firestore.googleapis.com/v1/projects/{ANOVA_PROJECT_ID}/databases/(default)/documents/{document_path}"
    )
}

fn document_name(document_path: &str) -> String {
    format!("projects/{ANOVA_PROJECT_ID}/databases/(default)/documents/{document_path}")
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunQueryRequest {
    structured_query: StructuredQuery,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StructuredQuery {
    from: Vec<CollectionSelector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    where_: Option<Filter>,
    #[serde(rename = "orderBy", skip_serializing_if = "Vec::is_empty")]
    order_by: Vec<Order>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectionSelector {
    collection_id: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum Filter {
    Composite {
        #[serde(rename = "compositeFilter")]
        composite_filter: CompositeFilter,
    },
    Field {
        #[serde(rename = "fieldFilter")]
        field_filter: FieldFilter,
    },
}

#[derive(Debug, Serialize)]
struct CompositeFilter {
    op: &'static str,
    filters: Vec<Filter>,
}

#[derive(Debug, Serialize)]
struct FieldFilter {
    field: FieldReference,
    op: &'static str,
    value: JsonValue,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FieldReference {
    field_path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Order {
    field: FieldReference,
    direction: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RunQueryItem {
    document: Option<Document>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Document {
    name: String,
    #[serde(default)]
    fields: Map<String, JsonValue>,
}

impl Document {
    fn id(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or("")
    }

    fn to_json(&self) -> JsonValue {
        map_fields_to_json(&self.fields)
    }
}

// ─── Firestore Value unwrapping ────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FirestoreValue {
    null_value: Option<JsonValue>,
    boolean_value: Option<bool>,
    integer_value: Option<String>,
    double_value: Option<f64>,
    timestamp_value: Option<String>,
    string_value: Option<String>,
    bytes_value: Option<String>,
    reference_value: Option<String>,
    array_value: Option<ArrayValue>,
    map_value: Option<MapValue>,
}

#[derive(Debug, Default, Deserialize)]
struct ArrayValue {
    #[serde(default)]
    values: Vec<FirestoreValue>,
}

#[derive(Debug, Default, Deserialize)]
struct MapValue {
    #[serde(default)]
    fields: Map<String, JsonValue>,
}

impl FirestoreValue {
    fn to_json(&self) -> JsonValue {
        if self.null_value.is_some() {
            return JsonValue::Null;
        }
        if let Some(b) = self.boolean_value {
            return JsonValue::Bool(b);
        }
        if let Some(i) = &self.integer_value {
            if let Ok(n) = i.parse::<i64>() {
                return JsonValue::Number(n.into());
            }
            return JsonValue::String(i.clone());
        }
        if let Some(f) = self.double_value {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return JsonValue::Number(n);
            }
        }
        if let Some(s) = &self.string_value {
            return JsonValue::String(s.clone());
        }
        if let Some(s) = &self.timestamp_value {
            return JsonValue::String(s.clone());
        }
        if let Some(s) = &self.reference_value {
            return JsonValue::String(s.clone());
        }
        if let Some(s) = &self.bytes_value {
            return JsonValue::String(s.clone());
        }
        if let Some(arr) = &self.array_value {
            let items: Vec<JsonValue> = arr.values.iter().map(|v| v.to_json()).collect();
            return JsonValue::Array(items);
        }
        if let Some(map) = &self.map_value {
            return map_fields_to_json(&map.fields);
        }
        JsonValue::Null
    }
}

fn map_fields_to_json(fields: &Map<String, JsonValue>) -> JsonValue {
    let mut out = Map::with_capacity(fields.len());
    for (k, v) in fields {
        let fv: FirestoreValue = serde_json::from_value(v.clone()).unwrap_or_default();
        out.insert(k.clone(), fv.to_json());
    }
    JsonValue::Object(out)
}

// ─── Query helpers ─────────────────────────────────────────────────────────────

fn user_recipes_query(uid: &str, limit: u32) -> RunQueryRequest {
    let user_profile_ref = document_name(&format!("user-profiles/{uid}"));
    RunQueryRequest {
        structured_query: StructuredQuery {
            from: vec![CollectionSelector {
                collection_id: "oven-recipes".into(),
            }],
            where_: Some(Filter::Composite {
                composite_filter: CompositeFilter {
                    op: "AND",
                    filters: vec![
                        Filter::Field {
                            field_filter: FieldFilter {
                                field: FieldReference {
                                    field_path: "userProfileRef".into(),
                                },
                                op: "EQUAL",
                                value: JsonValue::Object({
                                    let mut m = Map::new();
                                    m.insert(
                                        "referenceValue".into(),
                                        JsonValue::String(user_profile_ref),
                                    );
                                    m
                                }),
                            },
                        },
                        Filter::Field {
                            field_filter: FieldFilter {
                                field: FieldReference {
                                    field_path: "draft".into(),
                                },
                                op: "EQUAL",
                                value: JsonValue::Object({
                                    let mut m = Map::new();
                                    m.insert("booleanValue".into(), JsonValue::Bool(false));
                                    m
                                }),
                            },
                        },
                    ],
                },
            }),
            order_by: vec![Order {
                field: FieldReference {
                    field_path: "createdTimestamp".into(),
                },
                direction: "DESCENDING",
            }],
            limit: Some(limit),
        },
    }
}

fn favorites_query(limit: u32) -> RunQueryRequest {
    RunQueryRequest {
        structured_query: StructuredQuery {
            from: vec![CollectionSelector {
                collection_id: "favorite-oven-recipes".into(),
            }],
            where_: None,
            order_by: vec![Order {
                field: FieldReference {
                    field_path: "addedTimestamp".into(),
                },
                direction: "DESCENDING",
            }],
            limit: Some(limit),
        },
    }
}

fn oven_cooks_query(limit: u32) -> RunQueryRequest {
    oven_cooks_query_by("endedTimestamp", limit)
}

fn oven_cooks_query_by(order_field: &str, limit: u32) -> RunQueryRequest {
    RunQueryRequest {
        structured_query: StructuredQuery {
            from: vec![CollectionSelector {
                collection_id: "oven-cooks".into(),
            }],
            where_: None,
            order_by: vec![Order {
                field: FieldReference {
                    field_path: order_field.into(),
                },
                direction: "DESCENDING",
            }],
            limit: Some(limit),
        },
    }
}

// ─── High-level fetch functions ────────────────────────────────────────────────

fn parse_recipe_doc(doc: Document) -> anova_oven_api::Recipe {
    let id = doc.id().to_string();
    let json = doc.to_json();

    let title = json
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let steps: Vec<JsonValue> = json
        .get("steps")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let stages: Vec<anova_oven_api::Stage> = steps
        .iter()
        .filter(|s| {
            s.get("stepType")
                .and_then(|v| v.as_str())
                .map(|t| t == "stage")
                .unwrap_or(false)
        })
        .map(|s| stage_from_json(s))
        .collect();
    let stage_count = stages.len();

    anova_oven_api::Recipe {
        id,
        title,
        stage_count,
        stages,
    }
}

/// Fetch user's non-draft recipes from Firestore, converted to [`anova_oven_api::Recipe`].
/// Also fetches bookmarked recipes and merges them into a single deduplicated list.
pub async fn fetch_recipes(
    client: &reqwest::Client,
    session: &FirebaseSession,
) -> Result<Vec<anova_oven_api::Recipe>, FirestoreError> {
    let query = user_recipes_query(&session.uid, 100);
    let url = run_query_url("");
    let resp = client
        .post(&url)
        .bearer_auth(&session.id_token)
        .json(&query)
        .send()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(FirestoreError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(FirestoreError::Other(
            format!("Firestore runQuery (recipes) failed ({status}): {body}").into(),
        ));
    }
    let items: Vec<RunQueryItem> = resp
        .json()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;

    let mut recipes: Vec<anova_oven_api::Recipe> = items
        .into_iter()
        .filter_map(|item| item.document)
        .map(parse_recipe_doc)
        .collect();

    // Normalize all recipes for Anova compatibility (fan speed, etc)
    for recipe in &mut recipes {
        recipe.normalize();
    }

    // Merge in bookmarked recipes, skipping any already present by ID.
    // Auth errors from bookmarks are propagated so the caller can refresh;
    // other errors are swallowed (bookmarks are non-critical).
    let bookmarked = match fetch_bookmarked_recipes(client, session).await {
        Ok(b) => b,
        Err(e @ FirestoreError::Unauthorized) => return Err(e),
        Err(FirestoreError::Other(e)) => {
            warn!(error = %e, "[recipes] failed to fetch bookmarks");
            Vec::new()
        }
    };
    let own_ids: std::collections::HashSet<String> = recipes.iter().map(|r| r.id.clone()).collect();
    for mut recipe in bookmarked {
        if !own_ids.contains(&recipe.id) {
            recipe.normalize();
            recipes.push(recipe);
        }
    }

    Ok(recipes)
}

/// Fetch bookmarked recipes for the user from `users/{uid}/favorite-oven-recipes`.
async fn fetch_bookmarked_recipes(
    client: &reqwest::Client,
    session: &FirebaseSession,
) -> Result<Vec<anova_oven_api::Recipe>, FirestoreError> {
    let query = favorites_query(100);
    let parent_path = format!("users/{}", session.uid);
    let url = run_query_url(&parent_path);
    let resp = client
        .post(&url)
        .bearer_auth(&session.id_token)
        .json(&query)
        .send()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(FirestoreError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(FirestoreError::Other(
            format!("Firestore runQuery (bookmarks) failed ({status}): {body}").into(),
        ));
    }
    let items: Vec<RunQueryItem> = resp
        .json()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;

    // Extract recipe IDs from recipeRef fields.
    let recipe_ids: Vec<String> = items
        .into_iter()
        .filter_map(|item| item.document)
        .filter_map(|doc| {
            let json = doc.to_json();
            json.get("recipeRef")
                .and_then(|v| v.as_str())
                .and_then(|r| r.rsplit('/').next())
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
        .collect();

    // Fetch each referenced recipe document individually.
    let mut recipes = Vec::new();
    for id in &recipe_ids {
        let url = get_document_url(&format!("oven-recipes/{id}"));
        let Ok(resp) = client.get(&url).bearer_auth(&session.id_token).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(doc): Result<Document, _> = resp.json().await else {
            continue;
        };
        recipes.push(parse_recipe_doc(doc));
    }

    Ok(recipes)
}

/// Fetch cook history from Firestore, resolving recipe titles where possible.
pub async fn fetch_history(
    client: &reqwest::Client,
    session: &FirebaseSession,
    limit: u32,
) -> Result<Vec<anova_oven_api::HistoryEntry>, FirestoreError> {
    // Fetch oven-cooks subcollection.
    let query = oven_cooks_query(limit);
    let parent_path = format!("users/{}", session.uid);
    let url = run_query_url(&parent_path);
    let resp = client
        .post(&url)
        .bearer_auth(&session.id_token)
        .json(&query)
        .send()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(FirestoreError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(FirestoreError::Other(
            format!("Firestore runQuery (history) failed ({status}): {body}").into(),
        ));
    }
    let items: Vec<RunQueryItem> = resp
        .json()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;

    // Collect all unique recipe IDs referenced by cooks.
    struct CookInfo {
        recipe_id: Option<String>,
        ended_at: String,
        stage_count: usize,
    }

    let mut cook_infos: Vec<CookInfo> = Vec::new();
    let mut recipe_ids: Vec<String> = Vec::new();

    for item in items {
        let Some(doc) = item.document else { continue };
        let json = doc.to_json();

        let recipe_id = json
            .get("recipeRef")
            .and_then(|v| v.as_str())
            .and_then(|r| r.rsplit('/').next())
            .filter(|s| !s.is_empty())
            .map(String::from);

        let ended_at = json
            .get("endedTimestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let stages: Vec<JsonValue> = json
            .get("stages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let stage_count = stages
            .iter()
            .filter(|s| {
                s.get("stepType")
                    .and_then(|v| v.as_str())
                    .map(|t| t == "stage")
                    .unwrap_or(true)
            })
            .count();

        if let Some(ref id) = recipe_id {
            if !recipe_ids.contains(id) {
                recipe_ids.push(id.clone());
            }
        }

        cook_infos.push(CookInfo {
            recipe_id,
            ended_at,
            stage_count,
        });
    }

    // Fetch recipe titles for all referenced IDs.
    let title_map = fetch_recipe_titles(client, session, &recipe_ids).await;

    let entries = cook_infos
        .into_iter()
        .map(|c| {
            let recipe_title = c
                .recipe_id
                .as_deref()
                .and_then(|id| title_map.get(id))
                .cloned()
                .unwrap_or_else(|| "[manual]".into());
            anova_oven_api::HistoryEntry {
                recipe_title,
                ended_at: c.ended_at,
                stage_count: c.stage_count,
            }
        })
        .collect();

    Ok(entries)
}

/// Fetch recipe titles for a list of Firestore document IDs.
/// Missing or failed fetches are silently skipped.
async fn fetch_recipe_titles(
    client: &reqwest::Client,
    session: &FirebaseSession,
    ids: &[String],
) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for id in ids {
        let url = get_document_url(&format!("oven-recipes/{id}"));
        let Ok(resp) = client.get(&url).bearer_auth(&session.id_token).send().await else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }
        let Ok(doc): Result<Document, _> = resp.json().await else {
            continue;
        };
        let json = doc.to_json();
        if let Some(title) = json.get("title").and_then(|v| v.as_str()) {
            if !title.is_empty() {
                map.insert(id.clone(), title.to_string());
            }
        }
    }
    map
}

fn document_path_from_reference(reference: &str) -> Option<String> {
    let marker = "/documents/";
    if let Some(idx) = reference.find(marker) {
        let start = idx + marker.len();
        let path = reference.get(start..)?.trim_matches('/');
        if !path.is_empty() {
            return Some(path.to_string());
        }
        return None;
    }

    let trimmed = reference.trim_matches('/');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

async fn fetch_recipe_title_from_reference(
    client: &reqwest::Client,
    session: &FirebaseSession,
    reference: &str,
) -> Option<String> {
    let document_path = document_path_from_reference(reference)?;
    let url = get_document_url(&document_path);
    let Ok(resp) = client.get(&url).bearer_auth(&session.id_token).send().await else {
        return None;
    };
    if !resp.status().is_success() {
        return None;
    }
    let Ok(doc): Result<Document, _> = resp.json().await else {
        return None;
    };
    let json = doc.to_json();
    json.get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn extract_cook_title(json: &JsonValue) -> Option<String> {
    // Custom/user-authored cooks do not always include a recipeRef, but often
    // include a direct recipe title field in the cook document payload.
    const TITLE_POINTERS: &[&str] = &[
        "/recipeTitle",
        "/title",
        "/name",
        "/recipe/title",
        "/recipe/name",
        "/recipeSnapshot/title",
        "/recipeSnapshot/name",
        "/recipeInfo/title",
        "/recipeInfo/name",
    ];

    for ptr in TITLE_POINTERS {
        if let Some(title) = json.pointer(ptr).and_then(|v| v.as_str()) {
            let trimmed = title.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    fn collect_title_candidates(
        value: &JsonValue,
        path: &str,
        out: &mut Vec<(String, String)>,
        depth: usize,
    ) {
        if depth > 8 {
            return;
        }

        match value {
            JsonValue::Object(map) => {
                for (k, v) in map {
                    let child_path = if path.is_empty() {
                        format!("/{k}")
                    } else {
                        format!("{path}/{k}")
                    };

                    let key_lower = k.to_ascii_lowercase();
                    let path_lower = child_path.to_ascii_lowercase();
                    let recipe_like_path =
                        path_lower.contains("recipe") || path_lower.contains("cook");
                    let title_like_key = key_lower == "title"
                        || key_lower == "name"
                        || key_lower == "recipetitle"
                        || key_lower == "recipename";

                    if recipe_like_path && title_like_key && !path_lower.contains("/stages/") {
                        if let Some(s) = v.as_str() {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                out.push((child_path.clone(), trimmed.to_string()));
                            }
                        }
                    }

                    collect_title_candidates(v, &child_path, out, depth + 1);
                }
            }
            JsonValue::Array(items) => {
                for (idx, v) in items.iter().enumerate() {
                    let child_path = if path.is_empty() {
                        format!("/{idx}")
                    } else {
                        format!("{path}/{idx}")
                    };
                    collect_title_candidates(v, &child_path, out, depth + 1);
                }
            }
            _ => {}
        }
    }

    let mut candidates = Vec::new();
    collect_title_candidates(json, "", &mut candidates, 0);
    if let Some((_, title)) = candidates.into_iter().next() {
        return Some(title);
    }

    None
}

fn cook_title_candidates_for_log(json: &JsonValue) -> Vec<(String, String)> {
    fn collect(value: &JsonValue, path: &str, out: &mut Vec<(String, String)>, depth: usize) {
        if depth > 6 || out.len() >= 12 {
            return;
        }
        match value {
            JsonValue::Object(map) => {
                for (k, v) in map {
                    let child_path = if path.is_empty() {
                        format!("/{k}")
                    } else {
                        format!("{path}/{k}")
                    };
                    let key_lower = k.to_ascii_lowercase();
                    if (key_lower.contains("title") || key_lower.contains("name"))
                        && !child_path.to_ascii_lowercase().contains("/stages/")
                    {
                        if let Some(s) = v.as_str() {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                out.push((child_path.clone(), trimmed.to_string()));
                            }
                        }
                    }
                    collect(v, &child_path, out, depth + 1);
                }
            }
            JsonValue::Array(items) => {
                for (idx, v) in items.iter().enumerate() {
                    let child_path = if path.is_empty() {
                        format!("/{idx}")
                    } else {
                        format!("{path}/{idx}")
                    };
                    collect(v, &child_path, out, depth + 1);
                }
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    collect(json, "", &mut out, 0);
    out
}

fn extract_recipe_identity_candidates(json: &JsonValue) -> Vec<(String, String)> {
    const DIRECT_POINTERS: &[&str] = &[
        "/recipeRef",
        "/recipeId",
        "/recipe/id",
        "/recipe/ref",
        "/recipe/reference",
        "/recipeInfo/id",
        "/recipeInfo/ref",
        "/recipeInfo/reference",
        "/recipeSnapshot/id",
        "/recipeSnapshot/ref",
        "/recipeSnapshot/reference",
        "/sourceRecipeRef",
        "/sourceRecipeId",
    ];

    let mut out = Vec::new();

    for ptr in DIRECT_POINTERS {
        if let Some(v) = json.pointer(ptr).and_then(|v| v.as_str()) {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                out.push(((*ptr).to_string(), trimmed.to_string()));
            }
        }
    }

    fn collect(value: &JsonValue, path: &str, out: &mut Vec<(String, String)>, depth: usize) {
        if depth > 8 || out.len() >= 24 {
            return;
        }

        match value {
            JsonValue::Object(map) => {
                for (k, v) in map {
                    let child_path = if path.is_empty() {
                        format!("/{k}")
                    } else {
                        format!("{path}/{k}")
                    };

                    let key = k.to_ascii_lowercase();
                    let looks_like_recipe_key = key.contains("recipe")
                        && (key.contains("id")
                            || key.contains("ref")
                            || key.contains("path")
                            || key.contains("document"));

                    if looks_like_recipe_key {
                        if let Some(s) = v.as_str() {
                            let trimmed = s.trim();
                            if !trimmed.is_empty() {
                                out.push((child_path.clone(), trimmed.to_string()));
                            }
                        }
                    }

                    collect(v, &child_path, out, depth + 1);
                }
            }
            JsonValue::Array(items) => {
                for (idx, v) in items.iter().enumerate() {
                    let child_path = if path.is_empty() {
                        format!("/{idx}")
                    } else {
                        format!("{path}/{idx}")
                    };
                    collect(v, &child_path, out, depth + 1);
                }
            }
            _ => {}
        }
    }

    collect(json, "", &mut out, 0);
    out
}

fn choose_recipe_ref_and_id(candidates: &[(String, String)]) -> (Option<String>, Option<String>) {
    // Prefer values that look like Firestore document references first.
    let recipe_ref = candidates
        .iter()
        .map(|(_, v)| v)
        .find(|v| v.contains("/documents/") || v.contains("oven-recipes/"))
        .cloned();

    if let Some(reference) = recipe_ref {
        let recipe_id = document_path_from_reference(&reference)
            .and_then(|path| path.rsplit('/').next().map(String::from))
            .filter(|s| !s.is_empty());
        return (Some(reference), recipe_id);
    }

    // Fall back to plain IDs if present.
    let recipe_id = candidates
        .iter()
        .map(|(_, v)| v)
        .find(|v| {
            let looks_like_path = v.contains('/');
            let len_ok = (8..=64).contains(&v.len());
            !looks_like_path && len_ok
        })
        .cloned();

    (None, recipe_id)
}

fn debug_current_cook_logging_enabled() -> bool {
    std::env::var("ANOVA_DEBUG")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on" | "debug"
            )
        })
        .unwrap_or(false)
}

/// Fetch the most recent in-progress cook from Firestore, if any.
///
/// Queries `oven-cooks` ordered by `createdTimestamp DESC` to find documents
/// that lack `endedTimestamp` (indicating an in-progress cook). Resolves the
/// recipe title from the `recipeRef` field if present.
pub async fn fetch_current_cook(
    client: &reqwest::Client,
    session: &FirebaseSession,
) -> Result<Option<anova_oven_api::CurrentCook>, FirestoreError> {
    let parent_path = format!("users/{}", session.uid);
    let url = run_query_url(&parent_path);

    // Order by createdTimestamp to include in-progress cooks (which lack
    // endedTimestamp and would be excluded by orderBy endedTimestamp).
    let query = oven_cooks_query_by("createdTimestamp", 5);
    let resp = client
        .post(&url)
        .bearer_auth(&session.id_token)
        .json(&query)
        .send()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;

    if !resp.status().is_success() {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(FirestoreError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(FirestoreError::Other(
            format!("Firestore runQuery (current-cook) failed ({status}): {body}").into(),
        ));
    }

    let items: Vec<RunQueryItem> = resp
        .json()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;

    // Look for a document that lacks endedTimestamp, indicating in-progress.
    for item in &items {
        let Some(doc) = &item.document else { continue };
        let json = doc.to_json();
        let has_ended = json
            .get("endedTimestamp")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        if has_ended {
            continue;
        }

        if debug_current_cook_logging_enabled() {
            debug!(document = %doc.name, "[current-cook] in-progress document");
            match serde_json::to_string_pretty(&json) {
                Ok(pretty) => debug!(raw_json = %pretty, "[current-cook] raw json"),
                Err(e) => warn!(error = %e, "[current-cook] failed to pretty-print json"),
            }
        }

        // Found an in-progress cook — build structured response.
        let started_at = json
            .get("createdTimestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let identity_candidates = extract_recipe_identity_candidates(&json);
        let (recipe_ref, recipe_id) = choose_recipe_ref_and_id(&identity_candidates);

        // Resolve recipe title using recipeRef when possible, then fall back to
        // title-like fields embedded in the cook document itself.
        let title_from_ref = if let Some(reference) = recipe_ref.as_deref() {
            fetch_recipe_title_from_reference(client, session, reference).await
        } else if let Some(ref id) = recipe_id {
            // Backward-compatible fallback if only an ID-like value is available.
            let titles = fetch_recipe_titles(client, session, std::slice::from_ref(id)).await;
            titles.get(id).cloned()
        } else {
            None
        };
        let recipe_title = title_from_ref
            .or_else(|| extract_cook_title(&json))
            .unwrap_or_else(|| "[manual]".into());

        if recipe_title == "[manual]" {
            let candidate_titles = cook_title_candidates_for_log(&json);
            debug!(
                recipe_ref = ?recipe_ref,
                recipe_id = ?recipe_id,
                title_candidates = ?candidate_titles,
                identity_candidates = ?identity_candidates,
                "[current-cook] unresolved recipe title"
            );
        }

        let raw_stages: Vec<JsonValue> = json
            .get("stages")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        let stages: Vec<anova_oven_api::Stage> =
            raw_stages.iter().map(|s| stage_from_json(s)).collect();

        let cook_stage_count = stages.iter().filter(|s| s.kind == "cook").count();
        let total_stage_count = stages.len();

        return Ok(Some(anova_oven_api::CurrentCook {
            recipe_title,
            recipe_id,
            started_at,
            stages,
            cook_stage_count,
            total_stage_count,
        }));
    }

    Ok(None)
}

/// PATCH the `recipeRef` field on the `users/{uid}/oven-cooks/{cook_id}` document.
///
/// Uses `updateMask.fieldPaths=recipeRef` so only that field is touched (other fields
/// written by Anova's cloud — stages, timestamps, etc. — are preserved). Creates the
/// document if it does not yet exist.
pub async fn patch_cook_recipe_ref(
    client: &reqwest::Client,
    session: &FirebaseSession,
    cook_id: &str,
    recipe_id: &str,
) -> Result<(), FirestoreError> {
    let url = format!(
        "https://firestore.googleapis.com/v1/projects/{ANOVA_PROJECT_ID}/databases/(default)/documents/users/{uid}/oven-cooks/{cook_id}?updateMask.fieldPaths=recipeRef",
        uid = session.uid,
    );
    let recipe_ref = format!(
        "projects/{ANOVA_PROJECT_ID}/databases/(default)/documents/oven-recipes/{recipe_id}"
    );
    let body = serde_json::json!({
        "fields": {
            "recipeRef": {
                "referenceValue": recipe_ref,
            }
        }
    });
    let resp = client
        .patch(&url)
        .bearer_auth(&session.id_token)
        .json(&body)
        .send()
        .await
        .map_err(|e| FirestoreError::Other(e.into()))?;
    if !resp.status().is_success() {
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(FirestoreError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        return Err(FirestoreError::Other(
            format!("Firestore patch (cook recipeRef) failed ({status}): {body}").into(),
        ));
    }
    Ok(())
}

/// Convert a raw Firestore stage JSON object into [`anova_oven_api::Stage`].
fn stage_from_json(s: &JsonValue) -> anova_oven_api::Stage {
    let kind = s
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("cook")
        .to_string();

    let temperature_bulbs_mode = s
        .get("temperatureBulbs")
        .and_then(|tb| tb.get("mode"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let temperature_c = s
        .get("temperatureBulbs")
        .and_then(|tb| {
            let mode = tb.get("mode")?.as_str()?;
            tb.get(mode)?.get("setpoint")?.get("celsius")?.as_f64()
        })
        .unwrap_or(0.0) as f32;

    let duration_secs = s
        .get("timer")
        .and_then(|t| t.get("initial"))
        .and_then(|t| t.as_u64());

    let timer_added = s.get("timerAdded").and_then(|v| v.as_bool());

    let probe_added = s.get("probeAdded").and_then(|v| v.as_bool());

    let probe_target_c = s
        .get("temperatureProbe")
        .and_then(|tp| tp.get("setpoint"))
        .and_then(|sp| sp.get("celsius"))
        .and_then(|v| v.as_f64())
        .map(|v| v as f32);

    let steam_pct = s
        .get("steamGenerators")
        .and_then(|sg| {
            let mode = sg.get("mode")?.as_str()?;
            match mode {
                "steam-percentage" => sg.get("steamPercentage")?.get("setpoint")?.as_f64(),
                "relative-humidity" => sg.get("relativeHumidity")?.get("setpoint")?.as_f64(),
                _ => None,
            }
        })
        .unwrap_or(0.0) as f32;

    let fan_speed = s
        .get("fan")
        .and_then(|f| f.get("speed"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u8;

    let user_action_required = s.get("userActionRequired").and_then(|v| v.as_bool());

    let rack_position = s
        .get("rackPosition")
        .and_then(|v| v.as_u64())
        .map(|v| v as u8);

    let heating_element_top = s
        .get("heatingElements")
        .and_then(|he| he.get("top"))
        .and_then(|t| t.get("on"))
        .and_then(|v| v.as_bool());

    let heating_element_rear = s
        .get("heatingElements")
        .and_then(|he| he.get("rear"))
        .and_then(|r| r.get("on"))
        .and_then(|v| v.as_bool());

    let heating_element_bottom = s
        .get("heatingElements")
        .and_then(|he| he.get("bottom"))
        .and_then(|b| b.get("on"))
        .and_then(|v| v.as_bool());

    let vent_open = s
        .get("vent")
        .and_then(|v| v.get("open"))
        .and_then(|v| v.as_bool());

    let title = s
        .get("title")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .map(String::from);

    anova_oven_api::Stage {
        kind,
        temperature_c,
        temperature_bulbs_mode,
        duration_secs,
        timer_added,
        probe_added,
        probe_target_c,
        steam_pct,
        fan_speed,
        user_action_required,
        rack_position,
        heating_element_top,
        heating_element_rear,
        heating_element_bottom,
        vent_open,
        title,
    }
}
