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
//! anova-oven-cli temperature 180c   # local mode only, as are the
//! anova-oven-cli timer 1h30m        # other cook controls
//! ```

use anova_oven_api::{CurrentCook, HistoryEntry, OvenStatus, Recipe, RecipeSource};
use clap::{Parser, Subcommand, ValueEnum};

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
    /// Start a cook with a recipe
    Start {
        /// Recipe ID to start
        #[arg(long)]
        recipe_id: String,
    },
    /// Reload recipes (in local mode, re-read the recipes directory)
    UpdateRecipes,
    /// Start the next stage, e.g. when the oven is waiting for you (local mode)
    NextStage,
    /// Restart the current stage's timer (local mode)
    Timer {
        /// e.g. 90s, 45m, 1h30m or 1:30:00
        #[arg(value_parser = parse_duration)]
        duration: u64,
    },
    /// Change the current stage's temperature (local mode)
    Temperature {
        /// With its unit, e.g. 180c or 356f
        #[arg(value_parser = parse_temperature_c)]
        temperature: f32,
    },
    /// Turn the oven light on or off; only works mid-cook (local mode)
    Lamp { state: OnOff },
    /// Set the oven's display unit (local mode)
    TemperatureUnit { unit: Unit },
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum Unit {
    C,
    F,
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
            let url = format!("{server}/api/status");
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
            let url = format!("{server}/api/recipes");
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
            let url = format!("{server}/api/stop");
            let resp = client.post(&url).send().await?;
            if resp.status() == reqwest::StatusCode::NO_CONTENT {
                println!("Stop command sent.");
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("Server returned {status}: {body}").into());
            }
        }

        Command::Start { recipe_id } => {
            let url = format!("{server}/api/start");
            let resp = client
                .post(&url)
                .json(&serde_json::json!({ "recipe_id": recipe_id }))
                .send()
                .await?;
            if resp.status() == reqwest::StatusCode::NO_CONTENT {
                println!("Start command sent.");
            } else {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(format!("Server returned {status}: {body}").into());
            }
        }

        Command::UpdateRecipes => {
            let recipes: Vec<Recipe> = post(&client, &server, "/api/update-recipes", None)
                .await?
                .json()
                .await?;
            println!("{} recipes.", recipes.len());
        }

        Command::NextStage => {
            post(&client, &server, "/api/next-stage", None).await?;
            println!("Next stage command sent.");
        }

        Command::Timer { duration } => {
            let body = serde_json::json!({ "seconds": duration });
            post(&client, &server, "/api/timer", Some(body)).await?;
            println!("Timer set to {}.", format_duration(duration));
        }

        Command::Temperature { temperature } => {
            let body = serde_json::json!({ "celsius": temperature });
            post(&client, &server, "/api/temperature", Some(body)).await?;
            println!(
                "Temperature set to {:.0}°F ({temperature:.0}°C).",
                celcius_to_fahrenheit(temperature)
            );
        }

        Command::Lamp { state } => {
            let on = matches!(state, OnOff::On);
            post(
                &client,
                &server,
                "/api/lamp",
                Some(serde_json::json!({ "on": on })),
            )
            .await?;
            println!("Lamp {} command sent.", if on { "on" } else { "off" });
        }

        Command::TemperatureUnit { unit } => {
            let unit = match unit {
                Unit::C => "C",
                Unit::F => "F",
            };
            let body = serde_json::json!({ "unit": unit });
            post(&client, &server, "/api/temperature-unit", Some(body)).await?;
            println!("Display unit set to °{unit}.");
        }

        Command::History => {
            let url = format!("{server}/api/history");
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
            let cook_url = format!("{server}/api/current-cook");
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

/// POST to the server, turning an error status into an error with its body.
async fn post(
    client: &reqwest::Client,
    server: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<reqwest::Response, Box<dyn std::error::Error>> {
    let mut req = client.post(format!("{server}{path}"));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Server returned {status}: {body}").into());
    }
    Ok(resp)
}

/// `90s`, `45m`, `1h30m`, `1h 30m 10s`, `1:30` (m:ss) or `1:30:00` (h:mm:ss),
/// in seconds. A bare number is refused, since minutes and seconds are both
/// plausible.
fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim().to_ascii_lowercase();
    let secs = if s.contains(':') {
        let parts: Vec<u64> = s
            .split(':')
            .map(|p| p.parse::<u64>().map_err(|_| format!("bad duration `{s}`")))
            .collect::<Result<_, _>>()?;
        match parts[..] {
            [m, sec] => m * 60 + sec,
            [h, m, sec] => h * 3600 + m * 60 + sec,
            _ => return Err(format!("bad duration `{s}`")),
        }
    } else {
        let mut total = 0;
        let mut number = String::new();
        for c in s.chars().filter(|c| !c.is_whitespace()) {
            if c.is_ascii_digit() {
                number.push(c);
                continue;
            }
            let unit = match c {
                'h' => 3600,
                'm' => 60,
                's' => 1,
                _ => return Err(format!("bad duration `{s}`")),
            };
            let n: u64 = number.parse().map_err(|_| format!("bad duration `{s}`"))?;
            total += n * unit;
            number.clear();
        }
        if !number.is_empty() {
            return Err(format!("`{s}` needs a unit, e.g. {s}m or {s}s"));
        }
        total
    };
    if secs == 0 {
        return Err("the duration must be above zero".into());
    }
    Ok(secs)
}

/// `180c` or `356f` (case-insensitive, `°` allowed), in °C.
fn parse_temperature_c(s: &str) -> Result<f32, String> {
    let s = s.trim().to_ascii_lowercase().replace('°', "");
    let (number, to_c): (&str, fn(f32) -> f32) = if let Some(n) = s.strip_suffix('c') {
        (n, |c| c)
    } else if let Some(n) = s.strip_suffix('f') {
        (n, |f| (f - 32.0) / 1.8)
    } else {
        return Err(format!("`{s}` needs a unit, e.g. {s}c or {s}f"));
    };
    let value: f32 = number
        .trim()
        .parse()
        .map_err(|_| format!("bad temperature `{s}`"))?;
    Ok(to_c(value))
}

fn print_status(s: &OvenStatus) {
    println!("Mode:        {}", s.phase());
    if s.cook_done {
        println!("             Done; the oven is holding until stopped");
    } else if s.stage_transition_pending_user_action {
        println!("             Waiting for you to start the next stage");
    }
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
    println!(
        "  Source: {}",
        match r.source {
            RecipeSource::Own => "mine",
            RecipeSource::Bookmarked => "bookmark",
        }
    );
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
            let probe_target = s
                .current_stage(&c.stages)
                .and_then(|stage| stage.probe_target_c);
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
    let url = format!("{server}/api/status");
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

#[cfg(test)]
mod tests {
    use super::{parse_duration, parse_temperature_c};

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s"), Ok(90));
        assert_eq!(parse_duration("45m"), Ok(2700));
        assert_eq!(parse_duration("1h30m"), Ok(5400));
        assert_eq!(parse_duration("1h 30m 10s"), Ok(5410));
        assert_eq!(parse_duration("1:30"), Ok(90));
        assert_eq!(parse_duration("1:30:00"), Ok(5400));
        assert!(parse_duration("90").is_err());
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("5x").is_err());
    }

    #[test]
    fn temperatures() {
        assert_eq!(parse_temperature_c("180c"), Ok(180.0));
        assert_eq!(parse_temperature_c("212F"), Ok(100.0));
        assert_eq!(parse_temperature_c("100°C"), Ok(100.0));
        assert!(parse_temperature_c("180").is_err());
    }
}
