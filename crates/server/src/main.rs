//! `live-mcp`: the MCP server for Ableton Live (`live-mcp serve`), plus commands to install
//! the LiveMCP Remote Script and talk to it from the terminal.

mod audio_file;
mod browser;
mod clips;
mod devices;
mod ear;
mod ear_install;
mod harmony;
mod live;
mod masking;
mod mcp;
mod reference;
mod session;
mod snapshots;
mod tracks;
mod transport;
mod watch;

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
    /// Install the Remote Script into Live's User Library and the LiveMCP Ear plugin into the
    /// VST3 folder, and create the shared token.
    Install {
        /// Live's User Library folder (Settings → Library in Live). Defaults to the standard location.
        #[arg(long)]
        user_library: Option<PathBuf>,
        /// Folder to install the LiveMCP Ear plugin into. Defaults to ~/Library/Audio/Plug-Ins/VST3.
        #[arg(long)]
        vst3_dir: Option<PathBuf>,
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
    /// Print changes to observable properties of TARGET as they happen, one JSON line each, until interrupted.
    ///
    /// `describe` lists a target's observable properties.
    Watch {
        target: String,
        #[arg(required = true)]
        props: Vec<String>,
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
        Cmd::Install { user_library, vst3_dir } => install(&options, user_library, vst3_dir),
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
        Cmd::Watch { target, props } => watch(&options, &target, &props).await,
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

fn install(options: &Options, user_library: Option<PathBuf>, vst3_dir: Option<PathBuf>) -> Result<()> {
    let library = match user_library {
        Some(path) => path,
        None => script::default_user_library()
            .context("no default User Library location on this platform; pass --user-library")?,
    };
    // Resolve both destinations before writing anything.
    let ear_target = match ear_install::EMBEDDED {
        Some(bundle) => {
            let dir = vst3_dir.or_else(ear_install::default_vst3_dir).context(
                "no default VST3 folder on this platform; pass --vst3-dir to install the LiveMCP Ear plugin",
            )?;
            Some((bundle, dir))
        }
        None => None,
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
    let script_changed = report.status != InstallStatus::Unchanged;

    let mut ear_changed = false;
    match ear_target {
        Some((bundle, dir)) => {
            let ear = ear_install::install(&bundle, &dir)
                .with_context(|| format!("installing the LiveMCP Ear plugin into {}", dir.display()))?;
            match ear.status {
                ear_install::InstallStatus::Created => println!("Installed {}", ear.path.display()),
                ear_install::InstallStatus::Unchanged => println!("Already up to date: {}", ear.path.display()),
                ear_install::InstallStatus::Updated => println!("Updated {}", ear.path.display()),
            }
            ear_changed = ear.status != ear_install::InstallStatus::Unchanged;
        }
        None => println!(
            "This build doesn't include the LiveMCP Ear plugin, so `listen` won't have anything to hear\n\
             through. Build with `just build` to include it (see the README)."
        ),
    }

    if script_changed {
        println!(
            "\nNext: in Live, open Settings → Link, Tempo & MIDI and choose \"{}\" as a Control Surface\n\
             (Input/Output: None). If it was already selected, quit and restart Live: re-selecting it\n\
             does not reload the code. Then run `live-mcp status`.",
            script::FOLDER
        );
    }
    if ear_changed {
        println!(
            "\nLiveMCP Ear: restart Live if it is running, since Live keeps a loaded plugin's old code.\n\
             If Live doesn't list it under Plug-Ins → live-mcp, open Settings → Plug-Ins, turn on\n\
             \"Use VST3 Plug-In System Folders\" and click Rescan."
        );
    }
    Ok(())
}

/// Whether the installed LiveMCP Ear matches the one in this binary.
fn ear_plugin_state() -> &'static str {
    let Some(bundle) = ear_install::EMBEDDED else { return "not included in this build" };
    let Some(dir) = ear_install::default_vst3_dir() else { return "unknown (no default VST3 folder)" };
    let path = dir.join(bundle.name);
    match ear_install::is_current(&bundle, &path) {
        Ok(true) => "current",
        Ok(false) if path.exists() => "outdated",
        Ok(false) => "not installed",
        Err(_) => "unreadable",
    }
}

async fn status(options: &Options) -> Result<()> {
    let live = connect(options).await?;
    let info = live.info().await?;
    let health = live.status().await?;
    let stale = live.script_is_stale();
    let ear = ear_plugin_state();
    print(&json!({
        "address": live.addr(),
        "script": serde_json::to_value(live.hello())?,
        "script_current": !stale,
        "ear_plugin": ear,
        "live": info,
        "main_thread": health,
    }))?;
    if stale {
        eprintln!(
            "warning: the running Remote Script differs from the one in this binary. Run `live-mcp install`, \
             then quit and restart Live (re-selecting the control surface does not reload the code).",
        );
    }
    if matches!(ear, "outdated" | "not installed") {
        eprintln!("warning: the LiveMCP Ear plugin is {ear}. Run `live-mcp install`, then restart Live.");
    }
    Ok(())
}

async fn watch(options: &Options, target: &str, props: &[String]) -> Result<()> {
    let live = connect(options).await?;
    let target = parse_target(target)?;
    let mut names = std::collections::HashMap::new();
    for prop in props {
        let result = live.observe(&target, prop).await?;
        let subscription = result["subscription"].as_u64().context("unexpected observe response")?;
        names.insert(subscription, prop.clone());
        let value = result.get("value").cloned().unwrap_or(Value::Null);
        println!("{}", json!({"prop": prop, "value": value, "initial": true}));
    }
    // Live closes the connection when the control surface goes away, and its listeners with it.
    loop {
        let batch = live.events().await?;
        if batch.dropped > 0 {
            // Listed where they would have been: before the changes that followed them.
            println!("{}", json!({"dropped": batch.dropped}));
        }
        for event in batch.events {
            let mut line = json!({"prop": names.get(&event.subscription), "tick": event.tick});
            if let Some(value) = event.value {
                line["value"] = value;
            }
            println!("{line}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
