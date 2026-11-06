//! Per-user state shared with the Remote Script: `~/.live-mcp/{token,bridge.json,config.json}`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use live_mcp_protocol::Endpoint;

/// Overrides the state directory. Tests use it; Live itself never sets it, so
/// the Remote Script inside Live always uses `~/.live-mcp`.
pub const STATE_DIR_ENV: &str = "LIVE_MCP_STATE_DIR";

pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

pub fn default_state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(STATE_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".live-mcp")
}

pub fn token_path(state_dir: &Path) -> PathBuf {
    state_dir.join("token")
}

pub fn endpoint_path(state_dir: &Path) -> PathBuf {
    state_dir.join("bridge.json")
}

pub fn read_token(state_dir: &Path) -> io::Result<String> {
    let path = token_path(state_dir);
    let token = fs::read_to_string(&path)?.trim().to_owned();
    if token.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{} is empty", path.display())));
    }
    Ok(token)
}

/// Returns the token, creating it (readable only by the current user) if it does not exist yet.
pub fn ensure_token(state_dir: &Path) -> io::Result<String> {
    match read_token(state_dir) {
        Ok(token) => return Ok(token),
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.kind() == io::ErrorKind::InvalidData => {}
        Err(e) => return Err(e),
    }
    fs::create_dir_all(state_dir)?;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let token = hex::encode(bytes);

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(token_path(state_dir))?;
    writeln!(file, "{token}")?;
    Ok(token)
}

/// The endpoint the running script last advertised, if any. It can be stale if Live crashed.
pub fn read_endpoint(state_dir: &Path) -> Option<Endpoint> {
    let text = fs::read_to_string(endpoint_path(state_dir)).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("nested");
        let first = ensure_token(&state).unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(ensure_token(&state).unwrap(), first);
        assert_eq!(read_token(&state).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(token_path(&state)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
