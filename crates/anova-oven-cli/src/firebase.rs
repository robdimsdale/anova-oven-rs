//! Shared Firebase auth + Firestore HTTP helpers used by multiple subcommands.

use std::path::Path;

use anova_oven_firestore::{
    auth::{self, RefreshTokenResponse, SignInRequest, SignInResponse},
    firestore::{run_query_url, RunQueryResponse},
    queries, ANOVA_OVEN_API_KEY, ANOVA_PROJECT_ID,
};

/// Authenticated Firebase session.
pub struct Session {
    pub id_token: String,
    pub uid: String,
}

pub async fn authenticate(
    client: &reqwest::Client,
    email: Option<String>,
    password: Option<String>,
) -> Result<Session, Box<dyn std::error::Error>> {
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
    let token = token.trim().to_string();
    if token.is_empty() {
        return Ok(None);
    }

    eprintln!("Using cached refresh token...");
    let url = auth::refresh_token_url(ANOVA_OVEN_API_KEY);
    let body = auth::build_refresh_form(&token);
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

    if parsed.refresh_token != token {
        let _ = std::fs::write(&path, &parsed.refresh_token);
    }

    Ok(Some(Session {
        id_token: parsed.id_token,
        uid: parsed.user_id,
    }))
}

pub async fn run_query(
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
