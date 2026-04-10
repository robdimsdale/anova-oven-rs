//! `history` subcommand — fetch and display the user's cook history from
//! Firestore (`users/{uid}/oven-cooks`).
//!
//! Recipe names are resolved from the local cache (`~/.anova-programs/`). Any
//! recipe not yet cached is fetched on-demand from Firestore and added to the
//! local cache so future runs are instant.

use std::collections::HashMap;
use std::path::PathBuf;

use anova_oven_firestore::{
    firestore::{get_document_url, Document},
    queries,
    recipe::{OvenCook, OvenRecipe},
    ANOVA_PROJECT_ID,
};
use serde_json::Value;

use crate::firebase::{authenticate, run_query, Session};

const HISTORY_LIMIT: u32 = 50;

pub async fn run(
    email: Option<String>,
    password: Option<String>,
    limit: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let session = authenticate(&client, email, password).await?;
    eprintln!("Signed in as UID {}", session.uid);

    let q = queries::oven_cooks(&session.uid, limit.min(HISTORY_LIMIT));
    let docs = run_query(&client, &session, &q).await?;
    eprintln!("Found {} cook(s)\n", docs.len());

    let cooks: Vec<OvenCook> = docs
        .iter()
        .filter_map(|d| match OvenCook::from_document(d) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("  Warning: failed to parse {}: {e}", d.id());
                None
            }
        })
        .collect();

    // Build name map, fetching any missing recipes on-demand.
    let names = resolve_names(&client, &session, &cooks).await?;

    for cook in &cooks {
        print_cook(cook, &names);
    }

    Ok(())
}

/// Load the local recipe cache, then fetch any recipe IDs referenced by
/// `cooks` that aren't already cached. Missing recipes are stored to disk for
/// future runs.
async fn resolve_names(
    client: &reqwest::Client,
    session: &Session,
    cooks: &[OvenCook],
) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let out_dir = programs_dir();
    let cache_path = out_dir.join("_all-recipes.json");

    // Load existing cache.
    let mut cached: Vec<OvenRecipe> = if let Ok(contents) = std::fs::read_to_string(&cache_path) {
        serde_json::from_str(&contents).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Build map: firestore_id -> title.
    let mut names: HashMap<String, String> = cached
        .iter()
        .filter_map(|r| Some((r.firestore_id.clone()?, r.title.clone())))
        .filter(|(_, title)| !title.is_empty())
        .collect();

    // Collect recipe IDs referenced by cooks but missing from cache.
    let missing_ids: Vec<String> = cooks
        .iter()
        .filter_map(|c| c.recipe_id().map(String::from))
        .filter(|id| !names.contains_key(id))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    if missing_ids.is_empty() {
        return Ok(names);
    }

    eprintln!(
        "Fetching {} uncached recipe(s) from Firestore...",
        missing_ids.len()
    );

    for id in &missing_ids {
        let doc_path = format!("oven-recipes/{id}");
        match fetch_document(client, session, &doc_path).await {
            Ok(Some(recipe)) => {
                let title = recipe.title.clone();
                if !title.is_empty() {
                    names.insert(id.clone(), title);
                }
                cached.push(recipe);
            }
            Ok(None) => eprintln!("  Warning: recipe {id} not found"),
            Err(e) => eprintln!("  Warning: fetching recipe {id} failed: {e}"),
        }
    }

    // Persist updated cache.
    if !missing_ids.is_empty() {
        if let Ok(json) = serde_json::to_string_pretty(&cached) {
            std::fs::create_dir_all(&out_dir)?;
            let _ = std::fs::write(&cache_path, json);
        }
    }

    Ok(names)
}

async fn fetch_document(
    client: &reqwest::Client,
    session: &Session,
    document_path: &str,
) -> Result<Option<OvenRecipe>, Box<dyn std::error::Error>> {
    let url = get_document_url(ANOVA_PROJECT_ID, document_path);
    let resp = client
        .get(&url)
        .bearer_auth(&session.id_token)
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("getDocument failed ({status}): {body}").into());
    }
    let doc: Document = resp.json().await?;
    Ok(Some(OvenRecipe::from_document(&doc)?))
}

fn programs_dir() -> PathBuf {
    PathBuf::from(format!(
        "{}/.anova-programs",
        std::env::var("HOME").unwrap_or_default()
    ))
}

fn print_cook(cook: &OvenCook, names: &HashMap<String, String>) {
    let recipe_name = cook
        .recipe_id()
        .and_then(|id| names.get(id))
        .map(String::as_str)
        .unwrap_or("[custom program]");

    let ts = cook.ended_timestamp.as_deref().unwrap_or("unknown time");

    println!("{recipe_name}  [{ts}]");

    let stages: Vec<&Value> = cook
        .stages
        .iter()
        .filter(|s| {
            s.get("stepType")
                .and_then(|v| v.as_str())
                .map(|t| t == "stage")
                .unwrap_or(true)
        })
        .collect();

    if stages.is_empty() {
        println!("  (no stage data)");
    } else {
        for (i, stage) in stages.iter().enumerate() {
            let stage_type = stage
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            let temp = stage
                .get("temperatureBulbs")
                .and_then(|tb| {
                    let mode = tb.get("mode")?.as_str()?;
                    let sp = tb.get(mode)?.get("setpoint")?;
                    let f = sp.get("fahrenheit")?.as_f64()?;
                    let c = sp.get("celsius")?.as_f64()?;
                    Some(format!("{f:.0}\u{00b0}F / {c:.0}\u{00b0}C"))
                })
                .unwrap_or_else(|| "unknown temp".into());

            let timer = stage
                .get("timer")
                .and_then(|t| t.get("initial"))
                .and_then(|t| t.as_u64())
                .map(|secs| format!(" for {}:{:02}", secs / 60, secs % 60))
                .unwrap_or_default();

            let steam = stage
                .get("steamGenerators")
                .and_then(|sg| {
                    let mode = sg.get("mode")?.as_str()?;
                    match mode {
                        "steam-percentage" => {
                            let pct = sg
                                .get("steamPercentage")
                                .and_then(|p| p.get("setpoint"))
                                .and_then(|p| p.as_f64())?;
                            Some(format!(", steam {pct:.0}%"))
                        }
                        "relative-humidity" => {
                            let pct = sg
                                .get("relativeHumidity")
                                .and_then(|p| p.get("setpoint"))
                                .and_then(|p| p.as_f64())?;
                            Some(format!(", humidity {pct:.0}%"))
                        }
                        _ => None,
                    }
                })
                .unwrap_or_default();

            println!("  {}: {} at {temp}{timer}{steam}", i + 1, stage_type);
        }
    }
    println!();
}
