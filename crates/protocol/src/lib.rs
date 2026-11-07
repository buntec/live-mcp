//! Wire types shared by everything that talks to the LiveMCP Remote Script.
//!
//! The protocol is newline-delimited JSON-RPC 2.0; see `docs/protocol.md`.
//! Results are deliberately left as [`serde_json::Value`]: the Remote Script
//! is a generic Live API bridge, so result shapes depend on what was asked.

pub mod ear;

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Must match `PROTOCOL_VERSION` in `remote_script/LiveMCP/__init__.py`.
pub const PROTOCOL_VERSION: u32 = 1;

/// Must match `DEFAULT_PORT` in the Remote Script.
pub const DEFAULT_PORT: u16 = 19418;

pub mod codes {
    pub const PARSE: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    /// The Live API raised an exception.
    pub const LIVE: i64 = -32000;
    pub const UNAUTHORIZED: i64 = -32001;
    /// An `{"id": ...}` reference is unknown or has been evicted.
    pub const STALE_REF: i64 = -32003;
    pub const UNAVAILABLE: i64 = -32004;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id, method: method.into(), params }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: Option<u64>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl std::error::Error for RpcError {}

/// What a request operates on: a path from a root (`live_set tracks 0`), a
/// previously returned object id, or (inside a batch) the object returned by an
/// earlier op. The last two can be followed by a relative path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Target {
    Path(String),
    Ref {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    /// The Live object returned by op number `result` of the same batch.
    BatchResult {
        result: usize,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
}

impl Target {
    pub fn path(path: impl Into<String>) -> Self {
        Target::Path(path.into())
    }

    pub fn id(id: u64) -> Self {
        Target::Ref { id, path: None }
    }

    /// Inside a batch: the object returned by op `index`.
    pub fn batch_result(index: usize) -> Self {
        Target::BatchResult { result: index, path: None }
    }

    /// Appends a relative path, e.g. `clip_slots 0`.
    pub fn join(&self, rest: &str) -> Self {
        let join = |base: &str| if base.is_empty() { rest.to_owned() } else { format!("{base} {rest}") };
        match self {
            Target::Path(path) => Target::Path(join(path)),
            Target::Ref { id, path } => Target::Ref { id: *id, path: Some(join(path.as_deref().unwrap_or(""))) },
            Target::BatchResult { result, path } => {
                Target::BatchResult { result: *result, path: Some(join(path.as_deref().unwrap_or(""))) }
            }
        }
    }

    /// Wrap this target as an argument value (`{"$ref": target}`).
    pub fn as_arg(&self) -> Value {
        json!({ "$ref": self })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Path(path) => f.write_str(path),
            Target::Ref { id, path: None } => write!(f, "#{id}"),
            Target::Ref { id, path: Some(path) } => write!(f, "#{id} {path}"),
            Target::BatchResult { result, path: None } => write!(f, "@{result}"),
            Target::BatchResult { result, path: Some(path) } => write!(f, "@{result} {path}"),
        }
    }
}

/// Parses the compact text form: `live_set tracks 0`, `#123`, `#123 clip_slots 0`,
/// or `@0 clip_slots 0` (an earlier op's result, inside a batch).
impl FromStr for Target {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (sigil, rest) = match s.chars().next() {
            Some(c @ ('#' | '@')) => (c, &s[1..]),
            Some(_) => return Ok(Target::Path(s.split_whitespace().collect::<Vec<_>>().join(" "))),
            None => return Err("empty target".into()),
        };
        let mut parts = rest.splitn(2, char::is_whitespace);
        let number = parts.next().unwrap_or_default();
        let path = parts.next().map(str::trim).filter(|p| !p.is_empty()).map(String::from);
        if sigil == '#' {
            let id = number.parse::<u64>().map_err(|_| format!("invalid object id in {s:?}"))?;
            Ok(Target::Ref { id, path })
        } else {
            let result = number.parse::<usize>().map_err(|_| format!("invalid batch op index in {s:?}"))?;
            Ok(Target::BatchResult { result, path })
        }
    }
}

/// An object reference as the script returns it: `{"$obj": id, "type": ..., "fields": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjRef {
    #[serde(rename = "$obj")]
    pub id: u64,
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<serde_json::Map<String, Value>>,
}

impl ObjRef {
    pub fn target(&self) -> Target {
        Target::id(self.id)
    }
}

/// `{"$new": "Clip.MidiNoteSpecification", "kwargs": {...}}`: construct a class from the `Live` module.
pub fn new_live_object(class: &str, kwargs: Value) -> Value {
    json!({ "$new": class, "kwargs": kwargs })
}

/// `{"$tuple": [...]}`: many Live API methods require a tuple, not a list.
pub fn tuple(items: Vec<Value>) -> Value {
    json!({ "$tuple": items })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloParams {
    pub token: String,
    pub protocol: u32,
    pub client: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HelloResult {
    pub script_version: String,
    pub protocol: u32,
    pub code_hash: String,
}

/// Contents of `<state dir>/bridge.json`, written by the script when it starts listening.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Endpoint {
    pub port: u16,
    pub pid: u32,
    pub script_version: String,
    pub protocol: u32,
    pub code_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchOp {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// A change to an observed property, as `events` returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub subscription: u64,
    /// Increases across all of the script's events.
    pub seq: u64,
    /// The script's tick count when the listener fired.
    pub tick: u64,
    /// Unix time (seconds) when the listener fired.
    pub time: f64,
    /// The property's value when the listener fired, encoded like a `get` result. `None` when
    /// the observable has no property of that name (a clip's `notes`); `Some(Null)` is a null value.
    #[serde(default, deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// The listener fired while one of this connection's own requests was running.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub own: bool,
}

/// The result of `events`: the connection's unread events, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Events {
    pub events: Vec<Event>,
    /// Events lost because more were queued than the script keeps.
    #[serde(default)]
    pub dropped: u64,
}

/// Maps a present field to `Some`, even when it is `null`.
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_text_forms() {
        assert_eq!("live_set  tracks 0".parse::<Target>().unwrap(), Target::path("live_set tracks 0"));
        assert_eq!("#42".parse::<Target>().unwrap(), Target::id(42));
        assert_eq!(
            "#42 clip_slots 0".parse::<Target>().unwrap(),
            Target::Ref { id: 42, path: Some("clip_slots 0".into()) }
        );
        assert_eq!(
            "@1 clip_slots 0".parse::<Target>().unwrap(),
            Target::BatchResult { result: 1, path: Some("clip_slots 0".into()) }
        );
        assert!("#x".parse::<Target>().is_err());
        assert!("@-1".parse::<Target>().is_err());
        assert!("".parse::<Target>().is_err());
    }

    #[test]
    fn target_json_forms() {
        assert_eq!(serde_json::to_value(Target::path("live_set")).unwrap(), json!("live_set"));
        assert_eq!(serde_json::to_value(Target::id(7)).unwrap(), json!({"id": 7}));
        let parsed: Target = serde_json::from_value(json!({"id": 7, "path": "name"})).unwrap();
        assert_eq!(parsed, Target::Ref { id: 7, path: Some("name".into()) });
        assert_eq!(Target::id(7).as_arg(), json!({"$ref": {"id": 7}}));
        assert_eq!(serde_json::to_value(Target::batch_result(0)).unwrap(), json!({"result": 0}));
        let parsed: Target = serde_json::from_value(json!({"result": 2, "path": "name"})).unwrap();
        assert_eq!(parsed, Target::BatchResult { result: 2, path: Some("name".into()) });
    }

    #[test]
    fn display_round_trips() {
        for text in ["live_set tracks 0", "#5", "#5 clip_slots 1 clip", "@0", "@2 name"] {
            assert_eq!(text.parse::<Target>().unwrap().to_string(), text);
        }
    }

    #[test]
    fn join_appends_paths() {
        assert_eq!(
            Target::path("live_set tracks 0").join("clip_slots 1"),
            Target::path("live_set tracks 0 clip_slots 1")
        );
        assert_eq!(Target::id(3).join("name"), Target::Ref { id: 3, path: Some("name".into()) });
        assert_eq!(Target::batch_result(0).join("a").join("b").to_string(), "@0 a b");
    }

    #[test]
    fn event_distinguishes_null_from_absent_values() {
        let events: Events = serde_json::from_value(json!({"events": [
            {"subscription": 1, "seq": 1, "tick": 5, "time": 1.5, "value": null},
            {"subscription": 2, "seq": 2, "tick": 5, "time": 1.5, "own": true},
        ]}))
        .unwrap();
        assert_eq!(events.events[0].value, Some(Value::Null));
        assert!(!events.events[0].own);
        assert_eq!(events.events[1].value, None);
        assert!(events.events[1].own);
        assert_eq!(events.dropped, 0);
    }

    #[test]
    fn obj_ref_parses() {
        let r: ObjRef =
            serde_json::from_value(json!({"$obj": 9, "type": "Track", "fields": {"name": "Bass"}})).unwrap();
        assert_eq!(r.target(), Target::id(9));
        assert_eq!(r.fields.unwrap()["name"], "Bass");
    }
}
