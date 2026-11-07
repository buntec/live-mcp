//! The MCP server: typed tools for common work plus generic access to the Live Object Model.

use std::sync::Arc;

use live_mcp_bridge::{GetOptions, Options, script};
use live_mcp_protocol::{BatchOp, Target};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::browser::{self, BrowserCache, BrowserParams, LoadDeviceParams};
use crate::clips::{self, ReadClipParams, WriteClipParams};
use crate::devices::{self, DeviceParametersParams, SetParameterParams};
use crate::ear::{self, EarHub, EarsParams, ListenParams};
use crate::live::{Live, default_true};
use crate::reference::{self, AnalyzeFileParams};
use crate::session::{self, SessionParams};
use crate::snapshots::{self, CompareParams, SnapshotStore, SnapshotsParams};
use crate::tracks::{self, CreateTrackParams};
use crate::transport::{self, TransportParams};
use crate::watch::{self, EventsParams, ObserveParams, UnobserveParams, Watches};

/// Results larger than this are refused, with a hint to narrow the request.
const MAX_RESULT_CHARS: usize = 200_000;

const INSTRUCTIONS: &str = "\
Controls Ableton Live through the LiveMCP Remote Script. Start with `session` to see the set.

Typed tools (session, create_track, write_clip, read_clip, browser, load_device, device_parameters, \
set_parameter, transport) cover common work. Times and lengths are in beats (quarter notes). Tracks \
are 0-based indexes among the regular tracks, exact names, \"master\", or \"return N\".

The generic tools (get, set, call, describe, batch) reach the whole Live Object Model:
- A target is a path from `live_set` or `live_app`, like \"live_set tracks 0 clip_slots 1 clip\", or \
\"#ID\" for an object returned earlier as {\"$obj\": ID}, optionally followed by a relative path \
(\"#ID devices 0\").
- `describe` lists an object's properties, methods and nested types. Use it to explore.
- In values and arguments, {\"$ref\": target} passes a Live object, {\"$tuple\": [...]} a tuple, and \
{\"$new\": \"Clip.MidiNoteSpecification\", \"kwargs\": {...}} constructs a class from the Live module.
- `batch` runs many ops in one request and one undo step. In a batch, \"@N\" targets the object \
returned by op N (e.g. a clip created by op 0).

To hear the result, put the LiveMCP Ear plugin on a track (load_device with path \
[\"plugins\", \"live-mcp\", \"LiveMCP Ear\"]; the master track hears everything) and call `listen`. It plays \
in real time, so listen to a few bars at a time. `analyze_file` measures an audio file (such as a \
reference track) the same way. Both return a snapshot id; `compare` reports how two snapshots differ, \
e.g. a section before and after an edit, or your mix against a reference.

To notice changes made in Live (by the user, automation or playback), `observe` properties that \
`describe` lists as observable, such as live_set's tempo and is_playing or a track's mute and name, \
then call `events` for what changed since the last call (`wait_ms` waits for the next change). \
Events include changes your own calls made; nothing marks who made a change.

Live answers on its UI ticks (about 10 per second). The first call after a pause waits up to \
~100 ms; calls that follow quickly are fast. A call that times out may still have been applied: \
check the state instead of retrying.";

#[derive(Clone)]
pub struct LiveServer {
    live: Arc<Live>,
    browser_cache: Arc<BrowserCache>,
    hub: Arc<EarHub>,
    snapshots: Arc<SnapshotStore>,
    watches: Arc<Watches>,
    tool_router: ToolRouter<Self>,
}

type ToolResult = Result<String, String>;

fn respond(result: anyhow::Result<Value>) -> ToolResult {
    match result {
        Ok(value) => {
            let text = serde_json::to_string(&value).map_err(|e| e.to_string())?;
            if text.len() > MAX_RESULT_CHARS {
                return Err(format!(
                    "the result is too large ({} characters); narrow it with props, fields, offset/limit or a \
                     more specific target",
                    text.len()
                ));
            }
            Ok(text)
        }
        Err(e) => Err(format!("{e:#}")),
    }
}

fn parse_target(text: &str) -> anyhow::Result<Target> {
    text.parse::<Target>().map_err(anyhow::Error::msg)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetParams {
    /// Path from live_set/live_app ("live_set tracks 0"), "#ID", or "#ID relative path".
    pub target: String,
    /// Read these properties of the target and return them as an object.
    pub props: Option<Vec<String>>,
    /// For each Live object in the result, also read these properties.
    pub fields: Option<Vec<String>>,
    /// Page through a list: the result becomes {total, offset, items}.
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetParams {
    pub target: String,
    pub prop: String,
    /// The new value. {"$ref": target} passes a Live object.
    pub value: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CallParams {
    pub target: String,
    pub method: String,
    #[serde(default)]
    pub args: Vec<Value>,
    pub kwargs: Option<Map<String, Value>>,
    /// For each Live object in the result, also read these properties.
    pub fields: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeParams {
    pub target: String,
    /// Also read each property's current value.
    #[serde(default)]
    pub values: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchOpParam {
    /// "get", "set", "call" or "describe".
    pub method: String,
    /// The same parameters as the tool of that name; targets may also be "@N" (op N's result).
    pub params: Map<String, Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BatchParams {
    pub ops: Vec<BatchOpParam>,
    /// Record the whole batch as one undo step in Live (default true).
    #[serde(default = "default_true")]
    pub undo_step: bool,
    /// Stop at the first failing op (default true).
    #[serde(default = "default_true")]
    pub stop_on_error: bool,
}

#[tool_router(router = tool_router)]
impl LiveServer {
    /// Starts listening for LiveMCP Ear plugins. Failure is recorded and reported by the tools.
    pub async fn start_ear_hub(&self, state_dir: &std::path::Path) {
        self.hub.start(state_dir).await;
    }

    pub fn new(options: Options) -> Self {
        Self {
            live: Arc::new(Live::new(options)),
            browser_cache: Arc::new(BrowserCache::default()),
            hub: Arc::new(EarHub::default()),
            snapshots: Arc::new(SnapshotStore::default()),
            watches: Arc::new(Watches::default()),
            tool_router: Self::tool_router(),
        }
    }

    // ------------------------------------------------------------ typed tools

    #[tool(
        description = "Overview of the Live set: tempo, time signature, transport, loop, tracks (kind, mute/solo/arm, \
                       devices, Session clips), return tracks, master devices and scenes.",
        annotations(read_only_hint = true)
    )]
    async fn session(&self, Parameters(p): Parameters<SessionParams>) -> ToolResult {
        respond(session::session(&self.live, p).await)
    }

    #[tool(description = "Create a MIDI, audio or return track, optionally at an index and with a name.")]
    async fn create_track(&self, Parameters(p): Parameters<CreateTrackParams>) -> ToolResult {
        respond(tracks::create_track(&self.live, p).await)
    }

    #[tool(
        description = "Write MIDI notes to a clip in a Session slot (created if empty; existing notes replaced unless \
                       replace=false) or to a new Arrangement clip at arrangement_start. One undo step."
    )]
    async fn write_clip(&self, Parameters(p): Parameters<WriteClipParams>) -> ToolResult {
        respond(clips::write_clip(&self.live, p).await)
    }

    #[tool(
        description = "Read a clip: name, length, loop and markers, and its MIDI notes.",
        annotations(read_only_hint = true)
    )]
    async fn read_clip(&self, Parameters(p): Parameters<ReadClipParams>) -> ToolResult {
        respond(clips::read_clip(&self.live, p).await)
    }

    #[tool(
        description = "List or search Live's browser (instruments, effects, drums, sounds, plugins, ...). With no \
                       path, lists the roots; with a path, lists that folder; with a query, searches item names. \
                       Pass a result's path to load_device.",
        annotations(read_only_hint = true)
    )]
    async fn browser(&self, Parameters(p): Parameters<BrowserParams>) -> ToolResult {
        respond(browser::browser(&self.live, &self.browser_cache, p).await)
    }

    #[tool(description = "Load a browser item (instrument, effect, preset, drum kit) onto a track.")]
    async fn load_device(&self, Parameters(p): Parameters<LoadDeviceParams>) -> ToolResult {
        respond(browser::load_device(&self.live, p).await)
    }

    #[tool(
        description = "List a device's parameters with values, ranges or options, and display strings; for a Rack, also its \
                       chains and their devices.",
        annotations(read_only_hint = true)
    )]
    async fn device_parameters(&self, Parameters(p): Parameters<DeviceParametersParams>) -> ToolResult {
        respond(devices::device_parameters(&self.live, p).await)
    }

    #[tool(
        description = "Set a device parameter by index or name, to a number or one of its options. Works on devices \
                       inside Racks too."
    )]
    async fn set_parameter(&self, Parameters(p): Parameters<SetParameterParams>) -> ToolResult {
        respond(devices::set_parameter(&self.live, p).await)
    }

    #[tool(
        description = "Transport: play, continue, stop, fire or stop a clip, fire a scene, stop all clips; set tempo, \
                       position and loop. With no action, reports the transport state."
    )]
    async fn transport(&self, Parameters(p): Parameters<TransportParams>) -> ToolResult {
        respond(transport::transport(&self.live, p).await)
    }

    // -------------------------------------------------------------- listening

    #[tool(
        description = "Listen to the set through a LiveMCP Ear plugin (default: the one on the master track): plays \
                       `bars` bars from `start_bar` in real time and returns per-bar loudness (LUFS), peak, RMS, \
                       crest, five frequency bands (low <120 Hz, low_mid 120-500, mid 500-2k, high_mid 2-6k, high \
                       >6k, dBFS), stereo correlation and width, plus a section summary. With `tracks` (a list, \
                       or [\"all\"]) it listens through several Ears at once and reports which tracks share each \
                       frequency band (masking)."
    )]
    async fn listen(&self, Parameters(p): Parameters<ListenParams>) -> ToolResult {
        respond(ear::listen(&self.live, &self.hub, &self.snapshots, p).await)
    }

    #[tool(
        description = "Analyse an audio file (e.g. a reference track; WAV, AIFF, FLAC, MP3, AAC/M4A, Ogg): the same \
                       per-bar measurements as `listen` (give bpm for musical bars, otherwise 2-second sections), \
                       plus whole-file gated integrated loudness, loudness range and peak. Much faster than real time.",
        annotations(read_only_hint = true)
    )]
    async fn analyze_file(&self, Parameters(p): Parameters<AnalyzeFileParams>) -> ToolResult {
        respond(reference::analyze_file(&self.snapshots, p).await)
    }

    #[tool(
        description = "Compare two snapshots (from listen or analyze_file), optionally limited to bar ranges: \
                       loudness, peak, crest (dynamics), width, correlation, and tonal balance relative to each \
                       one's overall level, as a − b, with notes on the differences that matter. Equal-length \
                       selections are also compared bar by bar (e.g. before/after an edit).",
        annotations(read_only_hint = true)
    )]
    async fn compare(&self, Parameters(p): Parameters<CompareParams>) -> ToolResult {
        respond(snapshots::compare(&self.snapshots, p))
    }

    #[tool(
        description = "List this session's snapshots: what each listen or analyze_file captured.",
        annotations(read_only_hint = true)
    )]
    async fn snapshots(&self, Parameters(_p): Parameters<SnapshotsParams>) -> ToolResult {
        respond(Ok(snapshots::list(&self.snapshots)))
    }

    #[tool(
        description = "List the LiveMCP Ear plugins in the set: which track each is on, whether it is connected to \
                       this server, and its latest bar.",
        annotations(read_only_hint = true)
    )]
    async fn ears(&self, Parameters(p): Parameters<EarsParams>) -> ToolResult {
        respond(ear::ears(&self.live, &self.hub, p).await)
    }

    // -------------------------------------------------------------- listeners

    #[tool(description = "Watch properties of a Live object for changes (the ones `describe` lists as observable, \
                       e.g. live_set tempo, is_playing, current_song_time; a track's mute, solo, arm, name; \
                       live_set tracks). The listener follows the object, not the path: it keeps watching a track \
                       that moves. Returns subscription ids and current values; read changes with `events`.")]
    async fn observe(&self, Parameters(p): Parameters<ObserveParams>) -> ToolResult {
        respond(watch::observe(&self.live, &self.watches, p).await)
    }

    #[tool(
        description = "Changes to observed properties since the last call, oldest first: the value after each change \
                       and how long ago it happened, including changes this server made. wait_ms waits for the first change if there is none yet; latest_only keeps one \
                       entry per subscription (useful for fast-changing values such as current_song_time)."
    )]
    async fn events(&self, Parameters(p): Parameters<EventsParams>) -> ToolResult {
        respond(watch::events(&self.live, &self.watches, p).await)
    }

    #[tool(description = "Stop watching: remove these subscriptions, or all of them when none are given.")]
    async fn unobserve(&self, Parameters(p): Parameters<UnobserveParams>) -> ToolResult {
        respond(watch::unobserve(&self.live, &self.watches, p).await)
    }

    // ---------------------------------------------------------- generic tools

    #[tool(
        description = "Read any Live Object Model target, or some of its properties. Live objects come back as \
                       {\"$obj\": ID, \"type\": ...}; reuse them as \"#ID\".",
        annotations(read_only_hint = true)
    )]
    async fn get(&self, Parameters(p): Parameters<GetParams>) -> ToolResult {
        respond(
            async {
                let target = parse_target(&p.target)?;
                let opts = GetOptions {
                    props: p.props.unwrap_or_default(),
                    fields: p.fields.unwrap_or_default(),
                    offset: p.offset,
                    limit: p.limit,
                };
                Ok(self.live.bridge().await?.get(&target, opts).await?)
            }
            .await,
        )
    }

    #[tool(description = "Set a property of a Live object. Returns the value read back; some properties (e.g. \
                       current_song_time, is_playing) only change on Live's next tick, so the read-back can be stale.")]
    async fn set(&self, Parameters(p): Parameters<SetParams>) -> ToolResult {
        respond(
            async {
                let target = parse_target(&p.target)?;
                Ok(self.live.bridge().await?.set(&target, &p.prop, p.value).await?)
            }
            .await,
        )
    }

    #[tool(description = "Call a method of a Live object.")]
    async fn call(&self, Parameters(p): Parameters<CallParams>) -> ToolResult {
        respond(
            async {
                let mut params = json!({"target": parse_target(&p.target)?, "method": p.method, "args": p.args});
                if let Some(kwargs) = p.kwargs {
                    params["kwargs"] = Value::Object(kwargs);
                }
                if let Some(fields) = p.fields {
                    params["fields"] = json!(fields);
                }
                self.live.request("call", params).await
            }
            .await,
        )
    }

    #[tool(
        description = "List a Live object's properties, methods, nested types and observable properties.",
        annotations(read_only_hint = true)
    )]
    async fn describe(&self, Parameters(p): Parameters<DescribeParams>) -> ToolResult {
        respond(
            async {
                let target = parse_target(&p.target)?;
                Ok(self.live.bridge().await?.describe(&target, p.values).await?)
            }
            .await,
        )
    }

    #[tool(description = "Run several get/set/call/describe ops in one request and (by default) one undo step. \
                       \"@N\" targets the object returned by op N.")]
    async fn batch(&self, Parameters(p): Parameters<BatchParams>) -> ToolResult {
        respond(
            async {
                let mut ops = Vec::with_capacity(p.ops.len());
                for (index, op) in p.ops.into_iter().enumerate() {
                    let mut params = op.params;
                    // Accept the compact text forms ("#12 name", "@0") for targets.
                    if let Some(Value::String(text)) = params.get("target") {
                        let target = parse_target(text).map_err(|e| anyhow::anyhow!("op {index}: {e}"))?;
                        params.insert("target".into(), json!(target));
                    }
                    ops.push(BatchOp { method: op.method, params: Value::Object(params) });
                }
                Ok(self.live.bridge().await?.batch(&ops, p.undo_step, p.stop_on_error).await?)
            }
            .await,
        )
    }

    #[tool(
        description = "Connection check: Remote Script and Live versions, whether the installed script is current, \
                       and Live's tick rate.",
        annotations(read_only_hint = true)
    )]
    async fn status(&self) -> ToolResult {
        respond(
            async {
                let bridge = self.live.bridge().await?;
                let info = bridge.info().await?;
                let health = bridge.status().await?;
                let mut out = json!({"live": info, "bridge": health, "script_current": !bridge.script_is_stale()});
                if bridge.script_is_stale() {
                    out["warning"] = json!(format!(
                        "the Remote Script running in Live is not the one this server ships; run `live-mcp install` \
                     and restart Live (expected script hash {})",
                        script::source_hash()
                    ));
                }
                Ok(out)
            }
            .await,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for LiveServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("live-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}
