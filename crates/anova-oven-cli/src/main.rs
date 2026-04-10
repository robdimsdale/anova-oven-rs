use std::path::PathBuf;

use anova_oven_firestore::recipe::OvenRecipe;
use anova_oven_protocol::{parse_message, Event};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use http::{HeaderName, HeaderValue, Uri};
use tokio_websockets::ClientBuilder;

mod fetch_recipes;

#[derive(Parser)]
#[command(name = "anova-oven", about = "Control your Anova Precision Oven")]
struct Cli {
    /// Path to file containing the Anova API token
    #[arg(long, env = "ANOVA_TOKEN_FILE")]
    token_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show current oven state (one-shot)
    Status,
    /// Watch oven state continuously
    Watch,
    /// List cached cook programs (from ~/.anova-programs/)
    Programs,
    /// Download recipes from Firebase and cache them locally
    FetchRecipes {
        /// Anova account email (also ANOVA_EMAIL env var)
        #[arg(long, env = "ANOVA_EMAIL")]
        email: Option<String>,
        /// Anova account password (also ANOVA_PASSWORD env var)
        #[arg(long, env = "ANOVA_PASSWORD")]
        password: Option<String>,
        /// Also download bookmarked community recipes
        #[arg(long)]
        bookmarks: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::Status => {
            let token = read_token(cli.token_file)?;
            cmd_status(&token).await
        }
        Command::Watch => {
            let token = read_token(cli.token_file)?;
            cmd_watch(&token).await
        }
        Command::Programs => cmd_programs().await,
        Command::FetchRecipes {
            email,
            password,
            bookmarks,
        } => fetch_recipes::run(email, password, bookmarks).await,
    }
}

fn read_token(token_file: Option<PathBuf>) -> Result<String, Box<dyn std::error::Error>> {
    let token_path = token_file.unwrap_or_else(|| {
        PathBuf::from(format!(
            "{}/.anova-token",
            std::env::var("HOME").unwrap()
        ))
    });
    Ok(std::fs::read_to_string(&token_path)
        .map_err(|e| format!("Failed to read token from {}: {e}", token_path.display()))?
        .trim()
        .to_string())
}

async fn connect(token: &str) -> Result<impl StreamExt<Item = Result<tokio_websockets::Message, tokio_websockets::Error>>, Box<dyn std::error::Error>> {
    let uri = Uri::builder()
        .scheme("wss")
        .authority("devices.anovaculinary.io")
        .path_and_query(format!(
            "/?token={token}&supportedAccessories=APO&platform=android"
        ))
        .build()?;

    let (ws, _) = ClientBuilder::from_uri(uri)
        .add_header(
            HeaderName::from_static("sec-websocket-protocol"),
            HeaderValue::from_static("ANOVA_V2"),
        )?
        .connect()
        .await?;

    Ok(ws)
}

async fn cmd_status(token: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Connecting...");
    let mut ws = connect(token).await?;

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let payload = msg.as_payload();

        if let Ok(Event::ApoState(state)) = parse_message(payload) {
            print!("{state}");
            return Ok(());
        }
    }

    Err("Connection closed before receiving oven state".into())
}

async fn cmd_watch(token: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("Connecting...");
    let mut ws = connect(token).await?;
    eprintln!("Connected. Watching oven state (Ctrl-C to stop)...\n");

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let payload = msg.as_payload();

        match parse_message(payload) {
            Ok(Event::ApoState(state)) => print!("{state}"),
            Ok(Event::ApoWifiList { cooker_id }) => {
                if let Some(id) = cooker_id {
                    eprintln!("Oven discovered: {id}");
                }
            }
            Ok(Event::Response { request_id, status }) => {
                eprintln!("Response {request_id}: {status}");
            }
            Ok(_) => {}
            Err(e) => eprintln!("Failed to parse message: {e}"),
        }
    }

    Ok(())
}

async fn cmd_programs() -> Result<(), Box<dyn std::error::Error>> {
    let programs_dir = PathBuf::from(format!(
        "{}/.anova-programs",
        std::env::var("HOME").unwrap()
    ));

    if !programs_dir.exists() {
        eprintln!(
            "No programs directory found. Run `anova-oven fetch-recipes` to download your recipes."
        );
        return Ok(());
    }

    let mut entries: Vec<_> = std::fs::read_dir(&programs_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "json"))
        .filter(|e| {
            // Skip the combined dump files.
            e.path()
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| !n.starts_with('_'))
                .unwrap_or(false)
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());

    if entries.is_empty() {
        eprintln!("No program files found in {}", programs_dir.display());
        eprintln!("Run `anova-oven fetch-recipes` to download your recipes.");
        return Ok(());
    }

    for entry in entries {
        let path = entry.path();
        let name = path.file_stem().unwrap().to_string_lossy();
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  Warning: failed to read {}: {e}", path.display());
                continue;
            }
        };
        let recipe: OvenRecipe = match serde_json::from_str(&contents) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  Warning: failed to parse {}: {e}", path.display());
                continue;
            }
        };
        print_recipe(&name, &recipe);
    }

    Ok(())
}

fn print_recipe(slug: &str, recipe: &OvenRecipe) {
    let title = if recipe.title.is_empty() {
        slug
    } else {
        recipe.title.as_str()
    };
    println!("{slug}:");
    println!("  Title:  {title}");
    if !recipe.description.is_empty() {
        println!("  Desc:   {}", recipe.description);
    }

    let stages = recipe.cook_stages();
    println!("  Stages: {}", stages.len());
    for (i, stage) in stages.iter().enumerate() {
        let stage_type = stage
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("unknown");

        let temp = stage
            .get("temperatureBulbs")
            .and_then(|tb| {
                let mode = tb.get("mode")?.as_str()?;
                let sp = tb.get(mode)?.get("setpoint")?;
                Some(format!(
                    "{}\u{00b0}F / {}\u{00b0}C",
                    sp.get("fahrenheit")?,
                    sp.get("celsius")?
                ))
            })
            .unwrap_or_else(|| "unknown".into());

        let timer = stage
            .get("timer")
            .and_then(|t| t.get("initial"))
            .and_then(|t| t.as_u64())
            .map(|secs| format!(" ({}:{:02})", secs / 60, secs % 60))
            .unwrap_or_default();

        println!("    {}: {} at {}{timer}", i + 1, stage_type, temp);
    }
    println!();
}
