//! Connection to Live, batch helpers, and resolving tracks, devices and parameters.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use live_mcp_bridge::{Bridge, Options};
use live_mcp_protocol::{BatchOp, RpcError, Target};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

/// A lazily (re)connected bridge. A connection that Live closed (e.g. Live restarted)
/// is replaced before the next request is sent, so nothing is ever resent.
pub struct Live {
    options: Options,
    conn: Mutex<Option<Arc<Bridge>>>,
}

impl Live {
    pub fn new(options: Options) -> Self {
        Self { options, conn: Mutex::new(None) }
    }

    pub async fn bridge(&self) -> Result<Arc<Bridge>> {
        let mut conn = self.conn.lock().await;
        if let Some(bridge) = conn.as_ref().filter(|b| !b.is_closed()) {
            return Ok(bridge.clone());
        }
        let bridge = Arc::new(Bridge::connect(&self.options).await?);
        *conn = Some(bridge.clone());
        Ok(bridge)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Ok(self.bridge().await?.request(method, params).await?)
    }

    /// Runs `ops` as one batch and returns each op's outcome. Later ops still run after
    /// a failure, which suits reads where some properties may not exist.
    pub async fn batch_lenient(&self, ops: Vec<BatchOp>) -> Result<Vec<Result<Value, RpcError>>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let result = self.request("batch", json!({"ops": ops, "stop_on_error": false})).await?;
        parse_results(result)
    }

    /// Runs `ops` as one batch that stops at the first failure and fails as a whole.
    /// With `undo_step`, Live records everything as a single undo step.
    pub async fn batch(&self, ops: Vec<BatchOp>, undo_step: bool) -> Result<Vec<Value>> {
        if ops.is_empty() {
            return Ok(Vec::new());
        }
        let described: Vec<String> = ops.iter().map(describe_op).collect();
        let result = self.request("batch", json!({"ops": ops, "stop_on_error": true, "undo_step": undo_step})).await?;
        let mut values = Vec::new();
        for (index, outcome) in parse_results(result)?.into_iter().enumerate() {
            match outcome {
                Ok(value) => values.push(value),
                Err(error) => {
                    let done = if index == 0 { String::new() } else { format!(" (steps 1–{index} succeeded)") };
                    bail!("step {} failed: {} — {}{done}", index + 1, described[index], error.message)
                }
            }
        }
        Ok(values)
    }

    pub async fn get(&self, target: &Target) -> Result<Value> {
        self.request("get", json!({ "target": target })).await
    }

    pub async fn get_fields(&self, target: &Target, fields: &[&str]) -> Result<Value> {
        self.request("get", json!({ "target": target, "fields": fields })).await
    }

    /// Waits until Live's main thread has completed at least one more tick. Live applies
    /// transport commands and position changes on a later tick, so reads in the same tick can
    /// still see the old state.
    pub async fn wait_tick(&self) -> Result<()> {
        let ticks = |status: Value| status["ticks"].as_u64().unwrap_or(0);
        let start = ticks(self.request("status", json!({})).await?);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while ticks(self.request("status", json!({})).await?) <= start {
            if std::time::Instant::now() >= deadline {
                bail!("Live's main thread did not tick within 2 s (is a dialog open in Live?)");
            }
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        Ok(())
    }

    /// Number of items in a sequence property, without reading them.
    pub async fn count(&self, target: &Target) -> Result<usize> {
        let page = self.request("get", json!({"target": target, "offset": 0, "limit": 0})).await?;
        page["total"].as_u64().map(|n| n as usize).ok_or_else(|| anyhow!("{target} is not a list"))
    }
}

fn parse_results(result: Value) -> Result<Vec<Result<Value, RpcError>>> {
    let results = result.get("results").and_then(Value::as_array).context("malformed batch response")?;
    results
        .iter()
        .map(|entry| {
            if entry["ok"] == true {
                Ok(Ok(entry.get("result").cloned().unwrap_or(Value::Null)))
            } else {
                let error: RpcError =
                    serde_json::from_value(entry["error"].clone()).context("malformed batch error")?;
                Ok(Err(error))
            }
        })
        .collect()
}

fn describe_op(op: &BatchOp) -> String {
    let target = op.params.get("target").map(|t| match t {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    let detail = op.params.get("method").or_else(|| op.params.get("prop")).and_then(Value::as_str);
    match (target, detail) {
        (Some(t), Some(d)) => format!("{} {t} → {d}", op.method),
        (Some(t), None) => format!("{} {t}", op.method),
        _ => op.method.clone(),
    }
}

// ----------------------------------------------------------------- op builders

pub fn op(method: &str, params: Value) -> BatchOp {
    BatchOp { method: method.into(), params }
}

pub fn get_props(target: &Target, props: &[&str]) -> BatchOp {
    op("get", json!({ "target": target, "props": props }))
}

pub fn get_fields(target: &Target, fields: &[&str]) -> BatchOp {
    op("get", json!({ "target": target, "fields": fields }))
}

pub fn set(target: &Target, prop: &str, value: Value) -> BatchOp {
    op("set", json!({ "target": target, "prop": prop, "value": value }))
}

pub fn call(target: &Target, method: &str, args: Vec<Value>) -> BatchOp {
    op("call", json!({ "target": target, "method": method, "args": args }))
}

pub fn call_fields(target: &Target, method: &str, args: Vec<Value>, fields: &[&str]) -> BatchOp {
    op("call", json!({ "target": target, "method": method, "args": args, "fields": fields }))
}

// --------------------------------------------------------------- value helpers

/// A property read through `fields`, unless reading it failed.
pub fn field<'a>(obj: &'a Value, name: &str) -> Option<&'a Value> {
    obj.get("fields")?.get(name).filter(|v| v.get("$error").is_none() && !v.is_null())
}

pub fn field_str<'a>(obj: &'a Value, name: &str) -> &'a str {
    field(obj, name).and_then(Value::as_str).unwrap_or("")
}

pub fn field_bool(obj: &Value, name: &str) -> Option<bool> {
    field(obj, name).and_then(Value::as_bool)
}

pub fn field_f64(obj: &Value, name: &str) -> Option<f64> {
    field(obj, name).and_then(Value::as_f64)
}

pub fn obj_id(obj: &Value) -> Option<u64> {
    obj.get("$obj").and_then(Value::as_u64)
}

pub fn items(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// `{"$ref": target}`: pass a Live object as an argument.
pub fn obj_arg(target: &Target) -> Value {
    target.as_arg()
}

// ---------------------------------------------------------------- track refs

/// A track: its 0-based index among the regular tracks, its exact name, `"master"`,
/// or a return track as `"return 0"` / `"return A"`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TrackRef {
    Index(usize),
    Name(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Regular(usize),
    Return(usize),
    Master,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub target: Target,
    pub kind: TrackKind,
}

impl Track {
    pub fn regular(index: usize) -> Self {
        Self { target: Target::path(format!("live_set tracks {index}")), kind: TrackKind::Regular(index) }
    }

    pub fn return_track(index: usize) -> Self {
        Self { target: Target::path(format!("live_set return_tracks {index}")), kind: TrackKind::Return(index) }
    }

    pub fn master() -> Self {
        Self { target: Target::path("live_set master_track"), kind: TrackKind::Master }
    }

    /// How tools report the track back: a regular index, `"master"` or `"return N"`.
    pub fn label(&self) -> Value {
        match self.kind {
            TrackKind::Regular(i) => json!(i),
            TrackKind::Return(i) => json!(format!("return {i}")),
            TrackKind::Master => json!("master"),
        }
    }
}

impl TrackRef {
    pub async fn resolve(&self, live: &Live) -> Result<Track> {
        let name = match self {
            TrackRef::Index(i) => return checked_regular(live, *i).await,
            TrackRef::Name(name) => name.trim(),
        };
        if let Ok(i) = name.parse::<usize>() {
            return checked_regular(live, i).await;
        }
        let lower = name.to_lowercase();
        if lower == "master" || lower == "main" {
            return Ok(Track::master());
        }
        if let Some(rest) = lower.strip_prefix("return").map(str::trim).filter(|r| !r.is_empty()) {
            let index = match rest.parse::<usize>() {
                Ok(i) => Some(i),
                Err(_) if rest.len() == 1 && rest.as_bytes()[0].is_ascii_lowercase() => {
                    Some((rest.as_bytes()[0] - b'a') as usize)
                }
                Err(_) => None,
            };
            if let Some(index) = index {
                let count = live.count(&Target::path("live_set return_tracks")).await?;
                if index >= count {
                    bail!("there is no return track {index} (the set has {count})");
                }
                return Ok(Track::return_track(index));
            }
        }
        // Match by name, regular tracks first, then returns.
        let tracks = live.get_fields(&Target::path("live_set tracks"), &["name"]).await?;
        let returns = live.get_fields(&Target::path("live_set return_tracks"), &["name"]).await?;
        let mut matches: Vec<Track> = Vec::new();
        for (i, t) in items(&tracks).iter().enumerate() {
            if field_str(t, "name").to_lowercase() == lower {
                matches.push(Track::regular(i));
            }
        }
        for (i, t) in items(&returns).iter().enumerate() {
            if field_str(t, "name").to_lowercase() == lower {
                matches.push(Track::return_track(i));
            }
        }
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => {
                let names: Vec<&str> = items(&tracks).iter().map(|t| field_str(t, "name")).take(40).collect();
                bail!("no track named {name:?}; tracks are {names:?} (also \"master\", \"return 0\", ...)")
            }
            _ => {
                let labels: Vec<Value> = matches.iter().map(Track::label).collect();
                bail!("{} tracks are named {name:?}: {labels:?}; use an index", matches.len())
            }
        }
    }
}

async fn checked_regular(live: &Live, index: usize) -> Result<Track> {
    let count = live.count(&Target::path("live_set tracks")).await?;
    if index >= count {
        bail!("there is no track {index}: the set has {count} tracks (0–{})", count.saturating_sub(1));
    }
    Ok(Track::regular(index))
}

/// An item in a list (device, parameter): its 0-based index or its exact name.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ItemRef {
    Index(usize),
    Name(String),
}

impl ItemRef {
    /// Resolves against `list` (a target like `... devices`), returning the item's index and name.
    pub async fn resolve(&self, live: &Live, list: &Target, what: &str) -> Result<(usize, String)> {
        let all = live.get_fields(list, &["name"]).await?;
        let names: Vec<&str> = items(&all).iter().map(|v| field_str(v, "name")).collect();
        let index = match self {
            ItemRef::Index(i) => *i,
            ItemRef::Name(name) => {
                // A name wins over a number in a string ("808" is a common chain name); a string
                // that names nothing is read as an index, for clients that send numbers as strings.
                let lower = name.trim().to_lowercase();
                let hits: Vec<usize> =
                    names.iter().enumerate().filter(|(_, n)| n.to_lowercase() == lower).map(|(i, _)| i).collect();
                match (hits.as_slice(), name.trim().parse::<usize>()) {
                    ([i], _) => *i,
                    ([], Ok(i)) => i,
                    ([], Err(_)) => {
                        bail!("no {what} named {name:?}; there are {:?}", names.iter().take(60).collect::<Vec<_>>())
                    }
                    _ => bail!("several {what}s are named {name:?} (indexes {hits:?}); use an index"),
                }
            }
        };
        let name = names
            .get(index)
            .ok_or_else(|| anyhow!("there is no {what} {index}; there are {} ({:?})", names.len(), names))?;
        Ok((index, (*name).to_owned()))
    }
}

pub fn default_true() -> bool {
    true
}
