use std::path::PathBuf;

use anova_oven_protocol::{parse_message, Event};
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use http::{HeaderName, HeaderValue, Uri};
use tokio_websockets::ClientBuilder;

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
    /// List saved cook programs
    Programs,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let token_path = cli.token_file.unwrap_or_else(|| {
        PathBuf::from(format!(
            "{}/.anova-token",
            std::env::var("HOME").unwrap()
        ))
    });
    let token = std::fs::read_to_string(&token_path)
        .map_err(|e| format!("Failed to read token from {}: {e}", token_path.display()))?
        .trim()
        .to_string();

    match cli.command {
        Command::Status => cmd_status(&token).await,
        Command::Watch => cmd_watch(&token).await,
        Command::Programs => cmd_programs().await,
    }
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
    // User-saved programs are stored in Anova's Firebase/Firestore backend,
    // which is separate from the WebSocket device API. No public REST endpoint
    // exists for them.
    //
    // For now, we support local program files: JSON files in ~/.anova-programs/
    // that contain cook stage definitions compatible with CMD_APO_START.

    let programs_dir = PathBuf::from(format!(
        "{}/.anova-programs",
        std::env::var("HOME").unwrap()
    ));

    if !programs_dir.exists() {
        eprintln!(
            "No programs directory found. Create {} and add JSON program files.",
            programs_dir.display()
        );
        eprintln!();
        eprintln!("Example program file (e.g. roast-chicken.json):");
        eprintln!();
        print_example_program();
        return Ok(());
    }

    let mut found = false;
    let mut entries: Vec<_> = std::fs::read_dir(&programs_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "json"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let name = path.file_stem().unwrap().to_string_lossy();

        match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<Program>(&contents) {
                Ok(program) => {
                    found = true;
                    println!("{}:", name);
                    if let Some(desc) = &program.description {
                        println!("  {desc}");
                    }
                    println!("  Stages: {}", program.stages.len());
                    for (i, stage) in program.stages.iter().enumerate() {
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

                        let stage_type = stage
                            .get("type")
                            .and_then(|t| t.as_str())
                            .unwrap_or("unknown");

                        let timer = stage
                            .get("timer")
                            .and_then(|t| t.get("initial"))
                            .and_then(|t| t.as_u64())
                            .map(|secs| format!(" ({}:{:02})", secs / 60, secs % 60))
                            .unwrap_or_default();

                        println!(
                            "    {}: {} at {}{timer}",
                            i + 1,
                            stage_type,
                            temp,
                        );
                    }
                    println!();
                }
                Err(e) => eprintln!("  Warning: failed to parse {}: {e}", path.display()),
            },
            Err(e) => eprintln!("  Warning: failed to read {}: {e}", path.display()),
        }
    }

    if !found {
        eprintln!("No program files found in {}", programs_dir.display());
        eprintln!();
        eprintln!("Example program file (e.g. roast-chicken.json):");
        eprintln!();
        print_example_program();
    }

    Ok(())
}

fn print_example_program() {
    let example = r#"{
  "description": "Roast chicken at 425°F for 45 minutes",
  "stages": [
    {
      "stepType": "stage",
      "type": "preheat",
      "userActionRequired": false,
      "temperatureBulbs": {
        "dry": { "setpoint": { "fahrenheit": 425, "celsius": 218 } },
        "mode": "dry"
      },
      "heatingElements": { "top": { "on": false }, "bottom": { "on": false }, "rear": { "on": true } },
      "fan": { "speed": 100 },
      "vent": { "open": false },
      "rackPosition": 3
    },
    {
      "stepType": "stage",
      "type": "cook",
      "userActionRequired": false,
      "temperatureBulbs": {
        "dry": { "setpoint": { "fahrenheit": 425, "celsius": 218 } },
        "mode": "dry"
      },
      "heatingElements": { "top": { "on": false }, "bottom": { "on": false }, "rear": { "on": true } },
      "fan": { "speed": 100 },
      "vent": { "open": false },
      "rackPosition": 3,
      "timerAdded": true,
      "probeAdded": false,
      "timer": { "initial": 2700 }
    }
  ]
}"#;
    println!("{example}");
}

#[derive(serde::Deserialize)]
struct Program {
    description: Option<String>,
    stages: Vec<serde_json::Value>,
}
