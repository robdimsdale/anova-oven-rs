//! CLI client for the Anova Precision Oven local server.
//!
//! All data comes from the local `anova-oven-server` HTTP API.
//! No Firebase, Firestore, or WebSocket dependencies.
//!
//! # Usage
//! ```
//! anova-oven-cli --server http://localhost:8080 status
//! anova-oven-cli recipes
//! anova-oven-cli history
//! ```

use anova_oven_api::{HistoryEntry, OvenStatus, Recipe};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "anova-oven", about = "Control your Anova Precision Oven")]
struct Cli {
    /// Local server address (also ANOVA_SERVER env var)
    #[arg(long, env = "ANOVA_SERVER", default_value = "http://localhost:8080")]
    server: String,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show current oven state
    Status,
    /// List saved recipes
    Recipes,
    /// Show recent cook history
    History,
    /// Stop the current cook
    Stop,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();
    let raw = cli.server.trim_end_matches('/');
    let server = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };

    match cli.command {
        Command::Status => {
            let url = format!("{server}/status");
            let resp = client.get(&url).send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("Server returned {status}: {body}").into());
            }
            let status: OvenStatus = resp.json().await?;
            print_status(&status);
        }

        Command::Recipes => {
            let url = format!("{server}/recipes");
            let resp = client.get(&url).send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("Server returned {status}: {body}").into());
            }
            let recipes: Vec<Recipe> = resp.json().await?;
            if recipes.is_empty() {
                println!("No recipes found.");
            }
            for recipe in &recipes {
                print_recipe(recipe);
            }
        }

        Command::Stop => {
            let url = format!("{server}/stop");
            let resp = client.post(&url).send().await?;
            if resp.status() == reqwest::StatusCode::NO_CONTENT {
                println!("Stop command sent.");
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("Server returned {status}: {body}").into());
            }
        }

        Command::History => {
            let url = format!("{server}/history");
            let resp = client.get(&url).send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("Server returned {status}: {body}").into());
            }
            let history: Vec<HistoryEntry> = resp.json().await?;
            if history.is_empty() {
                println!("No cook history found.");
            }
            for entry in &history {
                print_history_entry(entry);
            }
        }
    }

    Ok(())
}

fn print_status(s: &OvenStatus) {
    println!("Mode:        {}", s.mode);
    println!(
        "Temperature: {:.1}°C",
        s.temperature_c
    );
    if let Some(target) = s.target_temperature_c {
        println!("Target:      {:.1}°C", target);
    }
    println!(
        "Timer:       {:02}:{:02} / {:02}:{:02}",
        s.timer_current_secs / 60,
        s.timer_current_secs % 60,
        s.timer_total_secs / 60,
        s.timer_total_secs % 60,
    );
    println!("Steam:       {:.0}%", s.steam_pct);
    println!("Door:        {}", if s.door_open { "open" } else { "closed" });
    println!("Water tank:  {}", if s.water_tank_empty { "EMPTY" } else { "ok" });
}

fn print_recipe(r: &Recipe) {
    println!("{}", r.title);
    println!("  ID:     {}", r.id);
    println!("  Stages: {}", r.stage_count);
    for (i, stage) in r.stages.iter().enumerate() {
        let duration = stage
            .duration_secs
            .map(|s| format!(" for {:02}:{:02}", s / 60, s % 60))
            .unwrap_or_default();
        println!(
            "    {}: {} at {:.0}°C{duration}, steam {:.0}%, fan {}%",
            i + 1,
            stage.kind,
            stage.temperature_c,
            stage.steam_pct,
            stage.fan_speed,
        );
    }
    println!();
}

fn print_history_entry(e: &HistoryEntry) {
    println!("{}", e.recipe_title);
    println!("  Ended:  {}", e.ended_at);
    println!("  Stages: {}", e.stage_count);
    println!();
}
