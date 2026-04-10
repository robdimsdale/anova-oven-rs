//! Firebase Auth + Firestore client, absorbed from `anova-oven-firestore`.
//!
//! Handles sign-in, token refresh, structured queries, and document parsing.
//! Converts Firestore documents into [`anova_oven_api`] types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};

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
) -> Result<Vec<anova_oven_api::Recipe>, Box<dyn std::error::Error + Send + Sync>> {
    let query = user_recipes_query(&session.uid, 100);
    let url = run_query_url("");
    let resp = client
        .post(&url)
        .bearer_auth(&session.id_token)
        .json(&query)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Firestore runQuery (recipes) failed ({status}): {body}").into());
    }
    let items: Vec<RunQueryItem> = resp.json().await?;

    let mut recipes: Vec<anova_oven_api::Recipe> = items
        .into_iter()
        .filter_map(|item| item.document)
        .map(parse_recipe_doc)
        .collect();

    // Merge in bookmarked recipes, skipping any already present by ID.
    let bookmarked = fetch_bookmarked_recipes(client, session)
        .await
        .unwrap_or_else(|e| {
            eprintln!("[recipes] Failed to fetch bookmarks: {e}");
            Vec::new()
        });
    let own_ids: std::collections::HashSet<String> = recipes.iter().map(|r| r.id.clone()).collect();
    for recipe in bookmarked {
        if !own_ids.contains(&recipe.id) {
            recipes.push(recipe);
        }
    }

    Ok(recipes)
}

/// Fetch bookmarked recipes for the user from `users/{uid}/favorite-oven-recipes`.
async fn fetch_bookmarked_recipes(
    client: &reqwest::Client,
    session: &FirebaseSession,
) -> Result<Vec<anova_oven_api::Recipe>, Box<dyn std::error::Error + Send + Sync>> {
    let query = favorites_query(100);
    let parent_path = format!("users/{}", session.uid);
    let url = run_query_url(&parent_path);
    let resp = client
        .post(&url)
        .bearer_auth(&session.id_token)
        .json(&query)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Firestore runQuery (bookmarks) failed ({status}): {body}").into());
    }
    let items: Vec<RunQueryItem> = resp.json().await?;

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
) -> Result<Vec<anova_oven_api::HistoryEntry>, Box<dyn std::error::Error + Send + Sync>> {
    // Fetch oven-cooks subcollection.
    let query = oven_cooks_query(limit);
    let parent_path = format!("users/{}", session.uid);
    let url = run_query_url(&parent_path);
    let resp = client
        .post(&url)
        .bearer_auth(&session.id_token)
        .json(&query)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Firestore runQuery (history) failed ({status}): {body}").into());
    }
    let items: Vec<RunQueryItem> = resp.json().await?;

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
                .unwrap_or_else(|| "[custom]".into());
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

/// Fetch the most recent in-progress cook from Firestore, if any.
///
/// Queries `oven-cooks` ordered by `createdTimestamp DESC` to find documents
/// that lack `endedTimestamp` (indicating an in-progress cook). Resolves the
/// recipe title from the `recipeRef` field if present.
pub async fn fetch_current_cook(
    client: &reqwest::Client,
    session: &FirebaseSession,
) -> Result<Option<anova_oven_api::CurrentCook>, Box<dyn std::error::Error + Send + Sync>> {
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
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Firestore runQuery (current-cook) failed ({status}): {body}").into());
    }

    let items: Vec<RunQueryItem> = resp.json().await?;

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

        // Found an in-progress cook — build structured response.
        let started_at = json
            .get("createdTimestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let recipe_id = json
            .get("recipeRef")
            .and_then(|v| v.as_str())
            .and_then(|r| r.rsplit('/').next())
            .filter(|s| !s.is_empty())
            .map(String::from);

        // Resolve recipe title.
        let recipe_title = if let Some(ref id) = recipe_id {
            let titles = fetch_recipe_titles(client, session, &[id.clone()]).await;
            titles.get(id).cloned().unwrap_or_else(|| "[custom]".into())
        } else {
            "[custom]".into()
        };

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
