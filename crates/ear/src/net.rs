//! The uplink to the MCP server: a background thread that connects, says hello, and streams
//! messages. While no server is listening it keeps the most recent bars and retries.

use std::collections::VecDeque;
use std::io::{BufWriter, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use live_mcp_protocol::ear::{EAR_DEFAULT_PORT, EAR_PROTOCOL_VERSION, EarEndpoint, EarMessage};

const RETRY_EVERY: Duration = Duration::from_secs(2);
/// Bars kept while disconnected, sent once a server is reachable.
const BACKLOG_BARS: usize = 256;
const QUEUE: usize = 4096;

/// Shared between the plugin and its task executor. Messages go to a background thread,
/// which is started on first use, so merely scanning the plugin starts no thread.
pub struct Uplink {
    instance: u64,
    sample_rate: Arc<AtomicU32>,
    state_dir: PathBuf,
    sender: OnceLock<SyncSender<EarMessage>>,
}

impl Uplink {
    pub fn new(instance: u64, state_dir: PathBuf) -> Self {
        Self { instance, sample_rate: Arc::new(AtomicU32::new(0)), state_dir, sender: OnceLock::new() }
    }

    pub fn instance(&self) -> u64 {
        self.instance
    }

    pub fn set_sample_rate(&self, sample_rate: f32) {
        self.sample_rate.store(sample_rate.to_bits(), Ordering::Relaxed);
    }

    /// Never blocks: if the background thread falls behind, the message is dropped.
    pub fn send(&self, message: EarMessage) {
        let sender = self.sender.get_or_init(|| {
            let (tx, rx) = mpsc::sync_channel(QUEUE);
            let worker = Worker {
                instance: self.instance,
                sample_rate: self.sample_rate.clone(),
                state_dir: self.state_dir.clone(),
                backlog: VecDeque::new(),
                last_heartbeat: None,
            };
            let _ = std::thread::Builder::new().name("LiveMCP Ear uplink".into()).spawn(move || worker.run(rx));
            tx
        });
        let _ = sender.try_send(message);
    }
}

/// `$LIVE_MCP_STATE_DIR`, else `~/.live-mcp` (the same directory the server uses).
pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("LIVE_MCP_STATE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    std::env::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".live-mcp")
}

struct Worker {
    instance: u64,
    sample_rate: Arc<AtomicU32>,
    state_dir: PathBuf,
    backlog: VecDeque<EarMessage>,
    last_heartbeat: Option<EarMessage>,
}

impl Worker {
    fn run(mut self, rx: Receiver<EarMessage>) {
        loop {
            match self.connect() {
                Some(mut conn) => {
                    if !self.stream(&mut conn, &rx) {
                        return; // the plugin is gone
                    }
                }
                None => {
                    // Not reachable: keep collecting until the next attempt.
                    let deadline = Instant::now() + RETRY_EVERY;
                    loop {
                        let wait = deadline.saturating_duration_since(Instant::now());
                        match rx.recv_timeout(wait) {
                            Ok(message) => self.keep(message),
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                }
            }
        }
    }

    fn keep(&mut self, message: EarMessage) {
        match message {
            EarMessage::Heartbeat { .. } => self.last_heartbeat = Some(message),
            _ => {
                if self.backlog.len() == BACKLOG_BARS {
                    self.backlog.pop_front();
                }
                self.backlog.push_back(message);
            }
        }
    }

    fn connect(&mut self) -> Option<BufWriter<TcpStream>> {
        let token = std::fs::read_to_string(self.state_dir.join("token")).ok()?.trim().to_owned();
        let port = std::fs::read_to_string(self.state_dir.join("ear.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<EarEndpoint>(&text).ok())
            .map_or(EAR_DEFAULT_PORT, |e| e.port);
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).ok()?;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
        let mut conn = BufWriter::new(stream);
        let hello = EarMessage::Hello {
            token,
            protocol: EAR_PROTOCOL_VERSION,
            instance: self.instance,
            plugin_version: env!("CARGO_PKG_VERSION").into(),
            sample_rate: f32::from_bits(self.sample_rate.load(Ordering::Relaxed)),
        };
        write(&mut conn, &hello).ok()?;
        let pending: Vec<EarMessage> = self.backlog.drain(..).chain(self.last_heartbeat.take()).collect();
        for message in &pending {
            write(&mut conn, message).ok()?;
        }
        conn.flush().ok()?;
        Some(conn)
    }

    /// Streams until the connection fails (returns true) or the plugin is gone (false).
    fn stream(&mut self, conn: &mut BufWriter<TcpStream>, rx: &Receiver<EarMessage>) -> bool {
        loop {
            let message = match rx.recv() {
                Ok(message) => message,
                Err(_) => return false,
            };
            let mut result = write(conn, &message);
            // Batch whatever else is already queued into the same write.
            while result.is_ok() {
                match rx.try_recv() {
                    Ok(next) => result = write(conn, &next),
                    Err(_) => break,
                }
            }
            if result.and_then(|_| conn.flush()).is_err() {
                self.keep(message);
                return true;
            }
        }
    }
}

fn write(conn: &mut BufWriter<TcpStream>, message: &EarMessage) -> std::io::Result<()> {
    serde_json::to_writer(&mut *conn, message)?;
    conn.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    use super::*;

    fn heartbeat(tag: u32) -> EarMessage {
        EarMessage::Heartbeat { tag, playing: false, beat: None }
    }

    #[test]
    fn says_hello_then_streams_and_flushes_its_backlog() {
        let state = std::env::temp_dir().join(format!("live-mcp-ear-test-{}", std::process::id()));
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("token"), "secret\n").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(state.join("ear.json"), format!(r#"{{"port": {port}, "pid": 1}}"#)).unwrap();

        let uplink = Uplink::new(77, state.clone());
        uplink.set_sample_rate(44_100.0);
        uplink.send(heartbeat(5));
        uplink.send(heartbeat(6));

        let (socket, _) = listener.accept().unwrap();
        let mut lines = BufReader::new(socket).lines();
        let mut next = || serde_json::from_str::<EarMessage>(&lines.next().unwrap().unwrap()).unwrap();
        match next() {
            EarMessage::Hello { token, instance, sample_rate, protocol, .. } => {
                assert_eq!((token.as_str(), instance, sample_rate, protocol), ("secret", 77, 44_100.0, 1));
            }
            other => panic!("expected hello, got {other:?}"),
        }
        // Both heartbeats arrive, whether they were sent before or after connecting.
        let mut tags = vec![next(), next()];
        tags.sort_by_key(|m| match m {
            EarMessage::Heartbeat { tag, .. } => *tag,
            _ => 0,
        });
        assert_eq!(tags, [heartbeat(5), heartbeat(6)]);
        drop(uplink);
        let _ = std::fs::remove_dir_all(state);
    }
}
