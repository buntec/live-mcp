//! Async client for the LiveMCP Remote Script running inside Ableton Live.
//!
//! ```no_run
//! # async fn demo() -> Result<(), live_mcp_bridge::BridgeError> {
//! use live_mcp_bridge::{Bridge, GetOptions, Options};
//! use live_mcp_protocol::Target;
//!
//! let live = Bridge::connect(&Options::default()).await?;
//! let tracks = live
//!     .get(&Target::path("live_set tracks"), GetOptions::fields(["name"]))
//!     .await?;
//! # Ok(()) }
//! ```

pub mod paths;
pub mod script;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use live_mcp_protocol::{
    BatchOp, DEFAULT_PORT, HelloParams, HelloResult, PROTOCOL_VERSION, Request, Response, RpcError, Target, codes,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Extra time the client waits beyond the timeout it asks the script to apply, so the
/// script's own (more informative) timeout error normally arrives first.
const CLIENT_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    #[error(
        "could not connect to the LiveMCP script at {addr}: {source}. Is Live running with the LiveMCP control surface selected?"
    )]
    Connect { addr: String, source: std::io::Error },
    #[error("timed out connecting to the LiveMCP script at {addr}")]
    ConnectTimeout { addr: String },
    #[error("could not read the token at {path}: {source}")]
    Token { path: PathBuf, source: std::io::Error },
    #[error("the LiveMCP script rejected the connection: {0}")]
    Unauthorized(String),
    /// The request may or may not have been applied. It is never retried automatically.
    #[error("{method} timed out after {after:?} (started on Live's main thread: {started}); outcome unknown, not retried", started = started.map_or("unknown".to_string(), |s| s.to_string()))]
    Timeout { method: String, after: Duration, started: Option<bool> },
    #[error("{method} failed: {error}")]
    Rpc { method: String, error: RpcError },
    #[error("connection to the LiveMCP script closed")]
    Closed,
    #[error("I/O error talking to the LiveMCP script: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected response from the LiveMCP script: {0}")]
    Protocol(String),
}

impl BridgeError {
    /// The JSON-RPC error code, if the script returned an error.
    pub fn code(&self) -> Option<i64> {
        match self {
            BridgeError::Rpc { error, .. } => Some(error.code),
            _ => None,
        }
    }
}

pub type Result<T, E = BridgeError> = std::result::Result<T, E>;

#[derive(Debug, Clone)]
pub struct Options {
    /// Holds `token` and `bridge.json`. Defaults to `~/.live-mcp`.
    pub state_dir: PathBuf,
    pub host: String,
    /// `None`: use the port advertised in `bridge.json`, else [`DEFAULT_PORT`].
    pub port: Option<u16>,
    pub connect_timeout: Duration,
    /// Default time to wait for Live's main thread per request.
    pub request_timeout: Duration,
    pub client_name: String,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            state_dir: paths::default_state_dir(),
            host: "127.0.0.1".into(),
            port: None,
            connect_timeout: Duration::from_secs(3),
            request_timeout: Duration::from_secs(30),
            client_name: format!("live-mcp {}", env!("CARGO_PKG_VERSION")),
        }
    }
}

impl Options {
    pub fn resolved_port(&self) -> u16 {
        self.port.or_else(|| paths::read_endpoint(&self.state_dir).map(|e| e.port)).unwrap_or(DEFAULT_PORT)
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct GetOptions {
    /// Read these properties of the target and return them as an object.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub props: Vec<String>,
    /// For each Live object in the result, also read these properties (one level deep).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
    /// Page through a sequence result; the result becomes `{total, offset, items}`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

impl GetOptions {
    pub fn props<I: IntoIterator<Item = S>, S: Into<String>>(props: I) -> Self {
        Self { props: props.into_iter().map(Into::into).collect(), ..Self::default() }
    }

    pub fn fields<I: IntoIterator<Item = S>, S: Into<String>>(fields: I) -> Self {
        Self { fields: fields.into_iter().map(Into::into).collect(), ..Self::default() }
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>;

/// One authenticated connection to the Remote Script. Requests may be issued
/// concurrently; responses are matched by id.
pub struct Bridge {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    pending: Pending,
    closed: Arc<AtomicBool>,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
    hello: HelloResult,
    addr: String,
    request_timeout: Duration,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Bridge {
    pub async fn connect(opts: &Options) -> Result<Self> {
        let token = paths::read_token(&opts.state_dir)
            .map_err(|source| BridgeError::Token { path: paths::token_path(&opts.state_dir), source })?;
        let addr = format!("{}:{}", opts.host, opts.resolved_port());
        let stream = tokio::time::timeout(opts.connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| BridgeError::ConnectTimeout { addr: addr.clone() })?
            .map_err(|source| BridgeError::Connect { addr: addr.clone(), source })?;
        stream.set_nodelay(true)?;
        let (read, write) = stream.into_split();

        let pending: Pending = Arc::default();
        let closed = Arc::new(AtomicBool::new(false));
        let reader = tokio::spawn(read_loop(read, pending.clone(), closed.clone()));

        let mut bridge = Self {
            writer: tokio::sync::Mutex::new(write),
            pending,
            closed,
            next_id: AtomicU64::new(1),
            reader,
            hello: HelloResult { script_version: String::new(), protocol: 0, code_hash: String::new() },
            addr,
            request_timeout: opts.request_timeout,
        };
        let params = HelloParams { token, protocol: PROTOCOL_VERSION, client: opts.client_name.clone() };
        let hello = bridge
            .request_with_timeout("hello", serde_json::to_value(params).expect("serialisable"), opts.connect_timeout)
            .await?;
        bridge.hello = serde_json::from_value(hello).map_err(|e| BridgeError::Protocol(format!("hello: {e}")))?;
        Ok(bridge)
    }

    /// What the script reported during the handshake.
    pub fn hello(&self) -> &HelloResult {
        &self.hello
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    /// True if the installed script is not the one embedded in this binary.
    pub fn script_is_stale(&self) -> bool {
        self.hello.code_hash != script::source_hash()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, self.request_timeout).await
    }

    /// Sends one request. `timeout` is how long the script may wait for Live's main thread.
    /// On timeout the outcome is unknown and the request is not retried.
    pub async fn request_with_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if self.is_closed() {
            return Err(BridgeError::Closed);
        }
        let mut params = match params {
            Value::Null => json!({}),
            Value::Object(map) => Value::Object(map),
            other => return Err(BridgeError::Protocol(format!("params must be an object, got {other}"))),
        };
        params.as_object_mut().expect("object").entry("timeout").or_insert(json!(timeout.as_secs_f64()));

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("pending lock").insert(id, tx);

        let mut line = serde_json::to_string(&Request::new(id, method, params)).expect("serialisable");
        line.push('\n');
        let write = async {
            let mut writer = self.writer.lock().await;
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await
        };
        if let Err(e) = write.await {
            self.pending.lock().expect("pending lock").remove(&id);
            return Err(e.into());
        }

        match tokio::time::timeout(timeout + CLIENT_GRACE, rx).await {
            Err(_) => {
                self.pending.lock().expect("pending lock").remove(&id);
                Err(BridgeError::Timeout { method: method.into(), after: timeout, started: None })
            }
            Ok(Err(_)) => Err(BridgeError::Closed),
            Ok(Ok(response)) => into_result(method, response),
        }
    }

    /// Script and Live versions. Runs on the main thread.
    pub async fn info(&self) -> Result<Value> {
        self.request("info", json!({})).await
    }

    /// Socket-thread health: queue depth, time since the last main-thread tick, current job.
    /// Answers even when Live's main thread is blocked.
    pub async fn status(&self) -> Result<Value> {
        self.request("status", json!({})).await
    }

    pub async fn get(&self, target: &Target, opts: GetOptions) -> Result<Value> {
        let mut params = serde_json::to_value(opts).expect("serialisable");
        params["target"] = json!(target);
        self.request("get", params).await
    }

    /// Sets a property and returns the value read back afterwards.
    pub async fn set(&self, target: &Target, prop: &str, value: Value) -> Result<Value> {
        let result = self.request("set", json!({"target": target, "prop": prop, "value": value})).await?;
        Ok(result.get("value").cloned().unwrap_or(Value::Null))
    }

    pub async fn call(&self, target: &Target, method: &str, args: Vec<Value>) -> Result<Value> {
        self.request("call", json!({"target": target, "method": method, "args": args})).await
    }

    /// Lists the target's public properties, methods and observable properties.
    pub async fn describe(&self, target: &Target, values: bool) -> Result<Value> {
        self.request("describe", json!({"target": target, "values": values})).await
    }

    /// Runs `ops` back to back in a single main-thread job. With `undo_step`, they are
    /// grouped into one undo step in Live.
    pub async fn batch(&self, ops: &[BatchOp], undo_step: bool, stop_on_error: bool) -> Result<Value> {
        self.request("batch", json!({"ops": ops, "undo_step": undo_step, "stop_on_error": stop_on_error})).await
    }
}

fn into_result(method: &str, response: Response) -> Result<Value> {
    let Some(error) = response.error else {
        return Ok(response.result.unwrap_or(Value::Null));
    };
    match error.code {
        codes::UNAUTHORIZED => Err(BridgeError::Unauthorized(error.message)),
        codes::TIMEOUT => Err(BridgeError::Timeout {
            method: method.into(),
            after: parse_after(&error.message),
            started: error.data.as_ref().and_then(|d| d.get("started")).and_then(Value::as_bool),
        }),
        _ => Err(BridgeError::Rpc { method: method.into(), error }),
    }
}

/// The script reports "timed out after 2.5s ..."; recover the duration for the error message.
fn parse_after(message: &str) -> Duration {
    message
        .split("after ")
        .nth(1)
        .and_then(|rest| rest.split('s').next())
        .and_then(|secs| secs.parse::<f64>().ok())
        .map(Duration::from_secs_f64)
        .unwrap_or_default()
}

async fn read_loop(read: OwnedReadHalf, pending: Pending, closed: Arc<AtomicBool>) {
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(response) = serde_json::from_str::<Response>(&line) else { continue };
        match response.id {
            Some(id) => {
                if let Some(tx) = pending.lock().expect("pending lock").remove(&id) {
                    let _ = tx.send(response);
                }
            }
            // An error without an id (e.g. "too many clients") concerns the whole connection.
            None => {
                for (_, tx) in pending.lock().expect("pending lock").drain() {
                    let _ = tx.send(response.clone());
                }
            }
        }
    }
    closed.store(true, Ordering::Release);
    pending.lock().expect("pending lock").clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_script_timeout_duration() {
        assert_eq!(parse_after("timed out after 2.5s waiting for Live's main thread"), Duration::from_millis(2500));
        assert_eq!(parse_after("something else"), Duration::ZERO);
    }

    #[test]
    fn timeout_error_carries_started_flag() {
        let response = Response {
            id: Some(1),
            result: None,
            error: Some(RpcError {
                code: codes::TIMEOUT,
                message: "timed out after 1s waiting for Live's main thread".into(),
                data: Some(json!({"method": "call", "started": true})),
            }),
        };
        match into_result("call", response) {
            Err(BridgeError::Timeout { started: Some(true), after, .. }) => assert_eq!(after, Duration::from_secs(1)),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
