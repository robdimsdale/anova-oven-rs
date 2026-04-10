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

use anova_oven_api::{CurrentCook, HistoryEntry, OvenStatus, Recipe};
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
    /// Show the current in-progress cook
    CurrentCook,
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

        Command::CurrentCook => {
            let cook_url = format!("{server}/current-cook");
            let cook_resp = client.get(&cook_url).send().await?;
            if cook_resp.status() == reqwest::StatusCode::NO_CONTENT {
                println!("No active cook.");
            } else if !cook_resp.status().is_success() {
                let status = cook_resp.status();
                let body = cook_resp.text().await.unwrap_or_default();
                return Err(format!("Server returned {status}: {body}").into());
            } else {
                let cook: CurrentCook = cook_resp.json().await?;
                // Also fetch live oven status for current temp/timer.
                let status = fetch_status(&client, &server).await;
                print_current_cook(&cook, status.as_ref());
            }
        }
    }

    Ok(())
}

fn print_status(s: &OvenStatus) {
    println!("Mode:        {}", s.phase());
    println!(
        "Temperature: {:.1}°F ({})",
        celcius_to_fahrenheit(s.current_temperature_c()),
        s.temperature_bulbs_mode
    );
    if let Some(target) = s.target_temperature_c {
        println!("Target:      {:.1}°F", celcius_to_fahrenheit(target));
    }
    if let Some(remaining) = s.timer_remaining_secs() {
        println!("Timer:       {}", format_duration(remaining));
    }
    println!("Steam:       {:.0}%", s.steam_pct);
    println!(
        "Door:        {}",
        if s.door_open { "open" } else { "closed" }
    );
    println!(
        "Water tank:  {}",
        if s.water_tank_empty { "EMPTY" } else { "ok" }
    );
}

fn print_recipe(r: &Recipe) {
    println!("{}", r.title);
    println!("  ID:     {}", r.id);
    println!("  Stages: {}", r.stage_count);
    for (i, stage) in r.stages.iter().enumerate() {
        print_stage(i, stage);
    }
    println!();
}

fn print_history_entry(e: &HistoryEntry) {
    println!("{}", e.recipe_title);
    println!("  Ended:  {}", e.ended_at);
    println!("  Stages: {}", e.stage_count);
    println!();
}

fn print_current_cook(c: &CurrentCook, status: Option<&OvenStatus>) {
    println!("Recipe:  {}", c.display_name());
    println!("Started: {}", c.started_at);

    if let Some(s) = status {
        println!("Status:  {}", s.phase());
        print!(
            "Temp:    {:.0}°F",
            celcius_to_fahrenheit(s.current_temperature_c())
        );
        if let Some(target) = s.target_temperature_c {
            print!(" → {:.0}°F", celcius_to_fahrenheit(target));
        }
        println!(" ({})", s.temperature_bulbs_mode);
        println!("Steam:   {:.0}%", s.steam_pct);

        // Show timer or probe depending on what's active.
        if let Some(remaining) = s.timer_remaining_secs() {
            println!("Timer:   {}", format_duration(remaining));
        }
        if let Some(probe_c) = s.probe_temperature_c {
            let probe_target = c.current_stage(s).and_then(|stage| stage.probe_target_c);
            if let Some(target) = probe_target {
                println!(
                    "Probe:   {:.0}°F → {:.0}°F",
                    celcius_to_fahrenheit(probe_c),
                    celcius_to_fahrenheit(target),
                );
            } else {
                println!("Probe:   {:.0}°F", celcius_to_fahrenheit(probe_c));
            }
        }
    }

    println!(
        "Stages:  {} cook / {} total",
        c.cook_stage_count, c.total_stage_count
    );
    for (i, stage) in c.stages.iter().enumerate() {
        print_stage(i, stage);
    }
}

fn print_stage(index: usize, stage: &anova_oven_api::Stage) {
    let duration = stage
        .duration_secs
        .map(|s| format!(" for {}", format_duration(s)))
        .unwrap_or_default();
    let probe = stage
        .probe_target_c
        .map(|c| format!(" probe→{:.0}°F", celcius_to_fahrenheit(c)))
        .unwrap_or_default();
    let mode = stage.temperature_bulbs_mode.as_deref().unwrap_or("dry");
    println!(
        "    {}: {} at {:.0}°F ({mode}){duration}{probe}, steam {:.0}%, fan {}%",
        index + 1,
        stage.kind,
        celcius_to_fahrenheit(stage.temperature_c),
        stage.steam_pct,
        stage.fan_speed,
    );
}

async fn fetch_status(client: &reqwest::Client, server: &str) -> Option<OvenStatus> {
    let url = format!("{server}/status");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json().await.ok()
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

fn celcius_to_fahrenheit(c: f32) -> f32 {
    c * 1.8 + 32.0
}
