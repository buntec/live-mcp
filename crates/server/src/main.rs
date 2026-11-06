//! `live-mcp`: the MCP server for Ableton Live (`live-mcp serve`), plus commands to install
//! the LiveMCP Remote Script and talk to it from the terminal.

mod audio_file;
mod browser;
mod clips;
mod devices;
mod ear;
mod live;
mod mcp;
mod reference;
mod session;
mod snapshots;
mod tracks;
mod transport;

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use live_mcp_bridge::script::{self, InstallStatus};
use live_mcp_bridge::{Bridge, GetOptions, Options, paths};
use live_mcp_protocol::{BatchOp, Target};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(name = "live-mcp", version, about = "Ableton Live bridge for MCP")]
struct Cli {
    /// Seconds Live's main thread may take per request.
    #[arg(long, global = true, default_value_t = 30.0)]
    timeout: f64,
    /// Override the port (default: the one in ~/.live-mcp/bridge.json, else 19418).
    #[arg(long, global = true)]
    port: Option<u16>,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the MCP server on stdin/stdout (this is what an MCP client launches).
    Serve,
    /// Install the Remote Script into Live's User Library and create the shared token.
    Install {
        /// Live's User Library folder (Settings → Library in Live). Defaults to the standard location.
        #[arg(long)]
        user_library: Option<PathBuf>,
    },
    /// Check the connection: handshake, script freshness, Live version, main-thread health.
    Status,
    /// Read a target, or some of its properties.
    ///
    /// TARGET is a path like "live_set tracks 0", or "#ID" / "#ID clip_slots 0" for a returned object.
    Get {
        target: String,
        /// Properties of TARGET to read, comma-separated.
        #[arg(long, value_delimiter = ',')]
        props: Vec<String>,
        /// Properties to read from each Live object in the result, comma-separated.
        #[arg(long, value_delimiter = ',')]
        fields: Vec<String>,
        #[arg(long)]
        offset: Option<usize>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// Set a property. VALUE is JSON (strings need quotes: '"Bass"').
    Set { target: String, prop: String, value: String },
    /// Call a method. Each ARG is JSON.
    Call {
        target: String,
        method: String,
        args: Vec<String>,
        /// Properties to read from each Live object in the result, comma-separated.
        #[arg(long, value_delimiter = ',')]
        fields: Vec<String>,
    },
    /// List a target's properties, methods and observable properties.
    Describe {
        target: String,
        /// Also read each property's current value.
        #[arg(long)]
        values: bool,
    },
    /// Run a JSON array of {"method", "params"} ops from FILE (or "-" for stdin) in one main-thread job.
    Batch {
        file: String,
        /// Group the ops into a single undo step in Live.
        #[arg(long)]
        undo_step: bool,
        /// Keep going after a failing op.
        #[arg(long)]
        keep_going: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    if !cli.timeout.is_finite() || cli.timeout <= 0.0 {
        bail!("--timeout must be positive");
    }
    let options =
        Options { port: cli.port, request_timeout: Duration::from_secs_f64(cli.timeout), ..Options::default() };
    match cli.command {
        Cmd::Serve => serve(options).await,
        Cmd::Install { user_library } => install(&options, user_library),
        Cmd::Status => status(&options).await,
        Cmd::Get { target, props, fields, offset, limit } => {
            let live = connect(&options).await?;
            let opts = GetOptions { props, fields, offset, limit };
            print(&live.get(&parse_target(&target)?, opts).await?)
        }
        Cmd::Set { target, prop, value } => {
            let live = connect(&options).await?;
            print(&live.set(&parse_target(&target)?, &prop, parse_json(&value)?).await?)
        }
        Cmd::Call { target, method, args, fields } => {
            let live = connect(&options).await?;
            let args = args.iter().map(|a| parse_json(a)).collect::<Result<Vec<_>>>()?;
            let mut params = json!({"target": parse_target(&target)?, "method": method, "args": args});
            if !fields.is_empty() {
                params["fields"] = json!(fields);
            }
            print(&live.request("call", params).await?)
        }
        Cmd::Describe { target, values } => {
            let live = connect(&options).await?;
            print(&live.describe(&parse_target(&target)?, values).await?)
        }
        Cmd::Batch { file, undo_step, keep_going } => {
            let text = if file == "-" {
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text)?;
                text
            } else {
                std::fs::read_to_string(&file).with_context(|| format!("reading {file}"))?
            };
            let ops: Vec<BatchOp> =
                serde_json::from_str(&text).context("batch file must be a JSON array of {method, params}")?;
            let live = connect(&options).await?;
            print(&live.batch(&ops, undo_step, !keep_going).await?)
        }
    }
}

async fn serve(options: Options) -> Result<()> {
    use rmcp::ServiceExt;
    let state_dir = options.state_dir.clone();
    let server = mcp::LiveServer::new(options);
    server.start_ear_hub(&state_dir).await;
    let server = server.serve(rmcp::transport::stdio()).await?;
    server.waiting().await?;
    Ok(())
}

fn install(options: &Options, user_library: Option<PathBuf>) -> Result<()> {
    let library = match user_library {
        Some(path) => path,
        None => script::default_user_library()
            .context("no default User Library location on this platform; pass --user-library")?,
    };
    let report = script::install(&library)?;
    paths::ensure_token(&options.state_dir)
        .with_context(|| format!("creating the token in {}", options.state_dir.display()))?;
    match &report.status {
        InstallStatus::Created => println!("Installed {}", report.path.display()),
        InstallStatus::Unchanged => println!("Already up to date: {}", report.path.display()),
        InstallStatus::Updated { backup } => {
            println!("Updated {} (previous version saved as {})", report.path.display(), backup.display())
        }
    }
    if report.status != InstallStatus::Unchanged {
        println!(
            "\nNext: in Live, open Settings → Link, Tempo & MIDI and choose \"{}\" as a Control Surface\n\
             (Input/Output: None). If it was already selected, quit and restart Live: re-selecting it\n\
             does not reload the code. Then run `live-mcp status`.",
            script::FOLDER
        );
    }
    Ok(())
}

async fn status(options: &Options) -> Result<()> {
    let live = connect(options).await?;
    let info = live.info().await?;
    let health = live.status().await?;
    let stale = live.script_is_stale();
    print(&json!({
        "address": live.addr(),
        "script": serde_json::to_value(live.hello())?,
        "script_current": !stale,
        "live": info,
        "main_thread": health,
    }))?;
    if stale {
        eprintln!(
            "warning: the running Remote Script differs from the one in this binary. Run `live-mcp install`, \
             then quit and restart Live (re-selecting the control surface does not reload the code).",
        );
    }
    Ok(())
}

async fn connect(options: &Options) -> Result<Bridge> {
    Ok(Bridge::connect(options).await?)
}

fn parse_target(text: &str) -> Result<Target> {
    text.parse::<Target>().map_err(anyhow::Error::msg)
}

fn parse_json(text: &str) -> Result<Value> {
    serde_json::from_str(text).with_context(|| format!("not valid JSON: {text} (strings need quotes, e.g. '\"Bass\"')"))
}

fn print(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
