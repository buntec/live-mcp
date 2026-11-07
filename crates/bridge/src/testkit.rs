//! Test support (feature `testkit`): runs the real Remote Script on the fake `Live`
//! module in `tests/fake_live/`, in a temporary state directory.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Bridge, Options, paths};

pub struct FakeLive {
    child: Child,
    state: tempfile::TempDir,
}

impl FakeLive {
    /// Starts the fake and waits until it listens. Returns `None` (after printing why)
    /// when `python3` is not available, so tests can skip.
    pub fn start() -> Option<Self> {
        if Command::new("python3").arg("--version").output().is_err() {
            eprintln!("skipping: python3 not found");
            return None;
        }
        let state = tempfile::tempdir().expect("tempdir");
        std::fs::write(state.path().join("config.json"), r#"{"port": 0, "ear_port": 0}"#).expect("write config");
        let child = Self::spawn(state.path());
        Some(Self { child, state })
    }

    /// Stops the fake and starts a new one in the same state directory, the way Live
    /// restarts: clients keep their token, but their connections and subscriptions end.
    pub fn restart(&mut self) {
        self.stop();
        self.child = Self::spawn(self.state.path());
    }

    fn spawn(state: &Path) -> Child {
        let runner = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_live/run.py");
        let child = Command::new("python3")
            .arg("-I")
            .arg("-B")
            .arg(runner)
            .env(paths::STATE_DIR_ENV, state)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn python3");
        let deadline = Instant::now() + Duration::from_secs(10);
        while paths::read_endpoint(state).is_none() {
            assert!(Instant::now() < deadline, "fake Live did not start listening");
            std::thread::sleep(Duration::from_millis(20));
        }
        child
    }

    /// Closing stdin stops the runner cleanly; the script removes its endpoint file.
    fn stop(&mut self) {
        drop(self.child.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(paths::endpoint_path(self.state.path()));
    }

    pub fn options(&self) -> Options {
        Options { state_dir: self.state_dir(), ..Options::default() }
    }

    /// Point a client here (or set `LIVE_MCP_STATE_DIR` to it) to reach this fake.
    pub fn state_dir(&self) -> PathBuf {
        self.state.path().to_path_buf()
    }

    pub async fn connect(&self) -> Bridge {
        Bridge::connect(&self.options()).await.expect("connect to fake Live")
    }
}

impl Drop for FakeLive {
    fn drop(&mut self) {
        self.stop();
    }
}
