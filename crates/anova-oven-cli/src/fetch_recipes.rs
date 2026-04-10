//! `fetch-recipes` subcommand — pulls the user's recipes from Firestore and
//! writes them as JSON files to `~/.anova-programs/`.

use std::path::PathBuf;

use anova_oven_firestore::{
    firestore::{get_document_url, Document},
    queries, recipe, ANOVA_PROJECT_ID,
};

use crate::firebase::{authenticate, run_query, Session};

const RECIPES_LIMIT: u32 = 100;
const BOOKMARKS_LIMIT: u32 = 100;

pub async fn run(
    email: Option<String>,
    password: Option<String>,
    include_bookmarks: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let client = reqwest::Client::new();
    let session = authenticate(&client, email, password).await?;
    eprintln!("Signed in as UID {}", session.uid);

    let out_dir = programs_dir();
    std::fs::create_dir_all(&out_dir)?;

    // --- User recipes ---
    eprintln!("Fetching recipes...");
    let query = queries::user_recipes(ANOVA_PROJECT_ID, &session.uid, RECIPES_LIMIT);
    let docs = run_query(&client, &session, &query).await?;
    let mut recipes = Vec::new();
    for d in &docs {
        match recipe::OvenRecipe::from_document(d) {
            Ok(r) => recipes.push(r),
            Err(e) => eprintln!("  Warning: failed to parse {}: {e}", d.id()),
        }
    }
    eprintln!("  Found {} recipe(s)", recipes.len());

    for r in &recipes {
        let filename = recipe_filename(r);
        let path = out_dir.join(&filename);
        let body = serde_json::to_string_pretty(r)?;
        std::fs::write(&path, body)?;
        let id = r.firestore_id.as_deref().unwrap_or("?");
        println!("  [{id}] \"{}\" -> {}", r.title, path.display());
    }

    let combined_path = out_dir.join("_all-recipes.json");
    std::fs::write(&combined_path, serde_json::to_string_pretty(&recipes)?)?;
    eprintln!("  Combined -> {}", combined_path.display());

    // --- Bookmarks (optional) ---
    if include_bookmarks {
        eprintln!("Fetching bookmarks...");
        let q = queries::favorite_recipes(&session.uid, BOOKMARKS_LIMIT);
        let bm_docs = run_query(&client, &session, &q).await?;
        eprintln!("  Found {} bookmark(s)", bm_docs.len());

        let mut bookmarks = Vec::new();
        for d in &bm_docs {
            match recipe::FavoriteOvenRecipe::from_document(d) {
                Ok(bm) => {
                    let Some(path) = bm.recipe_path().map(String::from) else {
                        eprintln!("  Warning: bookmark {} missing recipeRef", d.id());
                        continue;
                    };
                    match get_single_document(&client, &session, &path).await {
                        Ok(Some(full)) => bookmarks.push(full),
                        Ok(None) => eprintln!("  Warning: recipe {path} not found"),
                        Err(e) => eprintln!("  Warning: fetching {path} failed: {e}"),
                    }
                }
                Err(e) => eprintln!("  Warning: failed to parse bookmark {}: {e}", d.id()),
            }
        }

        let bm_path = out_dir.join("_bookmarks.json");
        std::fs::write(&bm_path, serde_json::to_string_pretty(&bookmarks)?)?;
        eprintln!("  Bookmarks -> {}", bm_path.display());
    }

    Ok(())
}

async fn get_single_document(
    client: &reqwest::Client,
    session: &Session,
    document_path: &str,
) -> Result<Option<recipe::OvenRecipe>, Box<dyn std::error::Error>> {
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
    Ok(Some(recipe::OvenRecipe::from_document(&doc)?))
}

fn programs_dir() -> PathBuf {
    PathBuf::from(format!(
        "{}/.anova-programs",
        std::env::var("HOME").unwrap_or_default()
    ))
}

fn recipe_filename(r: &recipe::OvenRecipe) -> String {
    let title = if r.title.is_empty() {
        r.firestore_id.as_deref().unwrap_or("untitled")
    } else {
        r.title.as_str()
    };
    let mut slug = String::with_capacity(title.len() + 5);
    let mut last_dash = true;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "untitled" } else { slug };
    format!("{slug}.json")
}
