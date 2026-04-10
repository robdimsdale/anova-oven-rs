//! `fetch-recipes` subcommand — pulls the user's recipes from Firestore and
//! writes them as JSON files to `~/.anova-programs/`.

use std::path::{Path, PathBuf};

use anova_oven_firestore::{
    auth::{self, RefreshTokenResponse, SignInRequest, SignInResponse},
    firestore::{get_document_url, run_query_url, RunQueryResponse},
    queries, recipe, ANOVA_OVEN_API_KEY, ANOVA_PROJECT_ID,
};

const RECIPES_LIMIT: u32 = 100;
const BOOKMARKS_LIMIT: u32 = 100;

/// Authenticated Firebase session: the ID token used for Firestore requests,
/// and the Firebase UID for query construction.
struct Session {
    id_token: String,
    uid: String,
}

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
    std::fs::write(
        &combined_path,
        serde_json::to_string_pretty(&recipes)?,
    )?;
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
                    // Fetch the referenced recipe document.
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

async fn authenticate(
    client: &reqwest::Client,
    email: Option<String>,
    password: Option<String>,
) -> Result<Session, Box<dyn std::error::Error>> {
    // Try refresh token first (avoids re-entering credentials).
    if let Some(session) = try_refresh_token(client).await? {
        return Ok(session);
    }

    let email = match email {
        Some(e) => e,
        None => std::env::var("ANOVA_EMAIL").or_else(|_| prompt("Anova email: "))?,
    };
    let password = match password {
        Some(p) => p,
        None => std::env::var("ANOVA_PASSWORD")
            .or_else(|_| rpassword::prompt_password("Anova password: "))?,
    };

    eprintln!("Signing in...");
    let url = auth::sign_in_url(ANOVA_OVEN_API_KEY);
    let resp = client
        .post(&url)
        .json(&SignInRequest::new(&email, &password))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("sign-in failed ({status}): {body}").into());
    }
    let parsed: SignInResponse = resp.json().await?;

    // Cache the refresh token for next time.
    if let Ok(home) = std::env::var("HOME") {
        let p = Path::new(&home).join(".anova-firebase-refresh-token");
        if let Err(e) = std::fs::write(&p, &parsed.refresh_token) {
            eprintln!("Warning: failed to cache refresh token to {}: {e}", p.display());
        } else {
            eprintln!("Cached refresh token to {}", p.display());
        }
    }

    Ok(Session {
        id_token: parsed.id_token,
        uid: parsed.local_id,
    })
}

async fn try_refresh_token(
    client: &reqwest::Client,
) -> Result<Option<Session>, Box<dyn std::error::Error>> {
    let Ok(home) = std::env::var("HOME") else {
        return Ok(None);
    };
    let path = Path::new(&home).join(".anova-firebase-refresh-token");
    let Ok(token) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let token = token.trim();
    if token.is_empty() {
        return Ok(None);
    }

    eprintln!("Using refresh token from {}...", path.display());
    let url = auth::refresh_token_url(ANOVA_OVEN_API_KEY);
    let body = auth::build_refresh_form(token);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        eprintln!("Refresh token rejected ({status}): {body_text}");
        eprintln!("Falling back to email/password.");
        return Ok(None);
    }
    let parsed: RefreshTokenResponse = resp.json().await?;

    // Write back the new refresh token if changed.
    if parsed.refresh_token != token {
        let _ = std::fs::write(&path, &parsed.refresh_token);
    }

    Ok(Some(Session {
        id_token: parsed.id_token,
        uid: parsed.user_id,
    }))
}

async fn run_query(
    client: &reqwest::Client,
    session: &Session,
    q: &queries::Query,
) -> Result<Vec<anova_oven_firestore::firestore::Document>, Box<dyn std::error::Error>> {
    let url = run_query_url(ANOVA_PROJECT_ID, &q.parent_path);
    let resp = client
        .post(&url)
        .bearer_auth(&session.id_token)
        .json(&q.body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("runQuery failed ({status}): {body}").into());
    }
    let items: RunQueryResponse = resp.json().await?;
    Ok(items.into_iter().filter_map(|i| i.document).collect())
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
    let doc: anova_oven_firestore::firestore::Document = resp.json().await?;
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

fn prompt(q: &str) -> std::io::Result<String> {
    use std::io::{BufRead, Write};
    let mut stdout = std::io::stdout();
    write!(stdout, "{q}")?;
    stdout.flush()?;
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}
