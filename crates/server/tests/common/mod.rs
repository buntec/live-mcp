//! Shared harness: an MCP client driving `live-mcp serve`, which talks to a fake Live.

#![allow(dead_code)]

use std::path::PathBuf;

use live_mcp_bridge::paths::STATE_DIR_ENV;
use live_mcp_bridge::testkit::FakeLive;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::{ConfigureCommandExt, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;

pub struct Harness {
    pub client: RunningService<RoleClient, ()>,
    pub fake: FakeLive,
}

impl Harness {
    pub async fn start() -> Option<Self> {
        let fake = FakeLive::start()?;
        let state = fake.state_dir();
        let command = tokio::process::Command::new(env!("CARGO_BIN_EXE_live-mcp")).configure(|cmd| {
            cmd.arg("serve").env(STATE_DIR_ENV, &state);
        });
        let client = ().serve(TokioChildProcess::new(command).expect("spawn live-mcp")).await.expect("initialize");
        Some(Self { client, fake })
    }

    /// Calls a tool; Ok with the parsed JSON result, or Err with the tool's error text.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        let arguments = args.as_object().cloned().expect("arguments must be an object");
        let result = self
            .client
            .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
            .await
            .expect("tools/call");
        let text = result.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone()).unwrap_or_default();
        if result.is_error == Some(true) {
            Err(text)
        } else {
            Ok(serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} returned non-JSON ({e}): {text}")))
        }
    }

    pub async fn ok(&self, name: &str, args: Value) -> Value {
        self.call(name, args).await.unwrap_or_else(|e| panic!("{name} failed: {e}"))
    }

    pub async fn err(&self, name: &str, args: Value) -> String {
        match self.call(name, args).await {
            Err(e) => e,
            Ok(v) => panic!("{name} unexpectedly succeeded: {v}"),
        }
    }
}

impl Harness {
    pub fn state_dir(&self) -> PathBuf {
        self.fake.state_dir()
    }
}
