use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use studio_core::client;

#[derive(Parser)]
#[command(
    name = "locals-studio",
    version,
    about = "Local MLX model library, servers and common API"
)]
struct Cli {
    #[arg(long, global = true, help = "Print machine-readable JSON")]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the persistent service (normally started automatically).
    Daemon,
    /// Global state: API, models, workers, downloads and runtime.
    Status,
    Models {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Search Hugging Face for published MLX models.
    Search {
        #[arg(default_value = "")]
        query: String,
    },
    /// Detailed metadata and available files for a Hugging Face repository.
    Info { repo_id: String },
    Download {
        #[command(subcommand)]
        command: DownloadCommand,
    },
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    /// Print the live service event stream as JSON Lines.
    Events,
    /// Send a typed service request, useful for scripts and integrations.
    #[command(hide = true)]
    Request {
        method: String,
        #[arg(default_value = "{}")]
        params: String,
    },
}

#[derive(Subcommand)]
enum ModelCommand {
    List,
    Scan {
        paths: Vec<String>,
    },
    Import {
        path: String,
    },
    /// Remove a registration; keeps original model files.
    Remove {
        id: String,
    },
}
#[derive(Subcommand)]
enum DownloadCommand {
    Add {
        repo_id: String,
        #[arg(long)]
        revision: Option<String>,
    },
    List,
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    Cancel {
        id: String,
    },
}
#[derive(Subcommand)]
enum ServerCommand {
    Start {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        #[arg(long, help = "Explicitly continue despite the memory estimate warning")]
        allow_memory_overcommit: bool,
    },
    Stop {
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
    },
    Status,
    Logs {
        id: String,
        #[arg(long, short)]
        follow: bool,
    },
}
#[derive(Subcommand)]
enum ConfigCommand {
    /// Read global settings, or a server profile selected by id/alias.
    Get {
        key: Option<String>,
        #[arg(long)]
        profile: Option<String>,
    },
    /// Set a JSON value; plain strings are also accepted.
    Set {
        key: String,
        value: String,
        #[arg(long)]
        profile: Option<String>,
    },
    /// List server profiles and their saved selection.
    Profiles,
}
#[derive(Subcommand)]
enum RuntimeCommand {
    Status,
    Install,
    Update,
}

#[tokio::main]
async fn main() {
    if let Err(error) = execute().await {
        eprintln!("locals-studio: {error:#}");
        std::process::exit(1);
    }
}

async fn execute() -> Result<()> {
    let cli = Cli::parse();
    if matches!(cli.command, Command::Daemon) {
        return studio_core::service::run().await;
    }
    if matches!(cli.command, Command::Events) {
        let mut events = client::subscribe().await?;
        loop {
            tokio::select! {event=events.recv()=>match event{Some(value)=>println!("{}",serde_json::to_string(&value)?),None=>break},_=tokio::signal::ctrl_c()=>break}
        }
        return Ok(());
    }
    let result = match cli.command {
        Command::Status => client::request("status", json!({})).await?,
        Command::Models { command } => match command {
            ModelCommand::List => client::request("models.list", json!({})).await?,
            ModelCommand::Scan { paths } => {
                client::request(
                    "models.scan",
                    if paths.is_empty() {
                        json!({})
                    } else {
                        json!({"paths":paths})
                    },
                )
                .await?
            }
            ModelCommand::Import { path } => {
                let path = std::fs::canonicalize(&path)
                    .with_context(|| format!("Cannot read model folder {path}"))?;
                client::request("models.import", json!({"path":path})).await?
            }
            ModelCommand::Remove { id } => {
                client::request("models.remove", json!({"id":id})).await?
            }
        },
        Command::Search { query } => client::request("search", json!({"query":query})).await?,
        Command::Info { repo_id } => {
            client::request("model.info", json!({"repo_id":repo_id})).await?
        }
        Command::Download { command } => match command {
            DownloadCommand::Add { repo_id, revision } => {
                client::request(
                    "download.add",
                    json!({"repo_id":repo_id,"revision":revision}),
                )
                .await?
            }
            DownloadCommand::List => client::request("download.list", json!({})).await?,
            DownloadCommand::Pause { id } => {
                client::request("download.pause", json!({"id":id})).await?
            }
            DownloadCommand::Resume { id } => {
                client::request("download.resume", json!({"id":id})).await?
            }
            DownloadCommand::Cancel { id } => {
                client::request("download.cancel", json!({"id":id})).await?
            }
        },
        Command::Server { command } => {
            match command {
                ServerCommand::Start {
                    id,
                    all,
                    allow_memory_overcommit,
                } => {
                    if id.is_none() && !all {
                        bail!("Pass a profile id/alias or --all to start the saved selection");
                    }
                    client::request("server.start",json!({"id":id,"all":all,"allow_memory_overcommit":allow_memory_overcommit})).await?
                }
                ServerCommand::Stop { id, all } => {
                    if id.is_none() && !all {
                        bail!("Pass a profile id/alias or --all");
                    }
                    client::request("server.stop", json!({"id":id,"all":all})).await?
                }
                ServerCommand::Status => client::request("server.status", json!({})).await?,
                ServerCommand::Logs { id, follow } => {
                    let profile = resolve_profile(&id).await?;
                    let id = profile["id"].as_str().context("Invalid profile")?;
                    let mut previous = String::new();
                    loop {
                        let logs = client::request("server.logs", json!({"id":id})).await?;
                        if !follow {
                            if cli.json {
                                println!("{}", serde_json::to_string(&logs)?);
                            } else {
                                print!("{}", log_text(&logs));
                            }
                            return Ok(());
                        }
                        let text = log_text(&logs);
                        if text != previous {
                            if let Some(extra) = text.strip_prefix(&previous) {
                                print!("{extra}");
                            } else {
                                print!("{text}");
                            }
                            previous = text;
                        }
                        tokio::select! {_=tokio::time::sleep(std::time::Duration::from_secs(1))=>{},_=tokio::signal::ctrl_c()=>return Ok(())}
                    }
                }
            }
        }
        Command::Config { command } => match command {
            ConfigCommand::Profiles => client::request("profiles.list", json!({})).await?,
            ConfigCommand::Get { key, profile } => {
                if let Some(profile) = profile {
                    let value = resolve_profile(&profile).await?;
                    if let Some(key) = key {
                        get_key(&value, &key)
                            .with_context(|| format!("Unknown profile key {key}"))?
                            .clone()
                    } else {
                        value
                    }
                } else {
                    client::request("config.get", json!({"key":key})).await?
                }
            }
            ConfigCommand::Set {
                key,
                value,
                profile,
            } => {
                let value = serde_json::from_str::<Value>(&value).unwrap_or(Value::String(value));
                if let Some(profile) = profile {
                    let mut profile = resolve_profile(&profile).await?;
                    set_key(&mut profile, &key, value)?;
                    client::request("profiles.save", json!({"profile":profile})).await?
                } else {
                    client::request("config.set", json!({"key":key,"value":value})).await?
                }
            }
        },
        Command::Runtime { command } => {
            client::request(
                match command {
                    RuntimeCommand::Status => "runtime.status",
                    RuntimeCommand::Install => "runtime.install",
                    RuntimeCommand::Update => "runtime.update",
                },
                json!({}),
            )
            .await?
        }
        Command::Request { method, params } => {
            client::request(
                &method,
                serde_json::from_str(&params).context("params must be JSON")?,
            )
            .await?
        }
        Command::Daemon | Command::Events => unreachable!(),
    };
    if cli.json {
        println!("{}", serde_json::to_string(&result)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&result)?);
    }
    Ok(())
}

async fn resolve_profile(identifier: &str) -> Result<Value> {
    let values = client::request("profiles.list", json!({})).await?;
    values
        .as_array()
        .context("Invalid profile list")?
        .iter()
        .find(|p| p["id"].as_str() == Some(identifier) || p["alias"].as_str() == Some(identifier))
        .cloned()
        .with_context(|| format!("No profile named {identifier}"))
}

fn log_text(value: &Value) -> String {
    if let Some(text) = value
        .as_str()
        .or_else(|| value["text"].as_str())
        .or_else(|| value["content"].as_str())
        .or_else(|| value["tail"].as_str())
    {
        text.to_owned()
    } else if let Some(lines) = value["lines"].as_array() {
        lines
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    } else {
        value.to_string() + "\n"
    }
}
fn get_key<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    key.split('.').try_fold(value, |v, k| v.get(k))
}
fn set_key(value: &mut Value, key: &str, replacement: Value) -> Result<()> {
    let keys: Vec<_> = key.split('.').collect();
    let mut cursor = value;
    for key in &keys[..keys.len() - 1] {
        if cursor.get(*key).is_none() {
            cursor[*key] = json!({});
        }
        cursor = cursor.get_mut(*key).context("Invalid configuration path")?;
    }
    cursor[keys[keys.len() - 1]] = replacement;
    Ok(())
}
