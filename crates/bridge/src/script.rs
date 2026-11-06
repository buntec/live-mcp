//! The Remote Script embedded in this binary, and installing it into Live's User Library.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// The exact bytes of `remote_script/LiveMCP/__init__.py`.
pub const SOURCE: &str = include_str!("../../../remote_script/LiveMCP/__init__.py");

/// Folder name under `Remote Scripts/`; it is also the name shown in Live's Control Surface menu.
pub const FOLDER: &str = "LiveMCP";

/// sha256 of [`SOURCE`]. The running script reports the hash of its own file in `hello`,
/// so a mismatch means the installed copy is stale.
pub fn source_hash() -> String {
    hex::encode(Sha256::digest(SOURCE.as_bytes()))
}

/// Live's default User Library location. Users can move it (Settings → Library),
/// in which case it must be passed explicitly.
pub fn default_user_library() -> Option<PathBuf> {
    let home = crate::paths::home_dir()?;
    if cfg!(target_os = "macos") {
        Some(home.join("Music").join("Ableton").join("User Library"))
    } else if cfg!(target_os = "windows") {
        Some(home.join("Documents").join("Ableton").join("User Library"))
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallStatus {
    Created,
    Unchanged,
    /// A different version was replaced; the old file was kept at `backup`.
    Updated {
        backup: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct InstallReport {
    pub path: PathBuf,
    pub status: InstallStatus,
}

/// Writes the script to `<user_library>/Remote Scripts/LiveMCP/__init__.py`.
pub fn install(user_library: &Path) -> io::Result<InstallReport> {
    if !user_library.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Ableton User Library not found at {}; pass its location explicitly \
                 (Live shows it under Settings → Library)",
                user_library.display()
            ),
        ));
    }
    let dir = user_library.join("Remote Scripts").join(FOLDER);
    fs::create_dir_all(&dir)?;
    let path = dir.join("__init__.py");
    let status = match fs::read(&path) {
        Ok(existing) if existing == SOURCE.as_bytes() => {
            return Ok(InstallReport { path, status: InstallStatus::Unchanged });
        }
        Ok(_) => {
            let backup = dir.join("__init__.py.bak");
            fs::copy(&path, &backup)?;
            InstallStatus::Updated { backup }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => InstallStatus::Created,
        Err(e) => return Err(e),
    };
    fs::write(&path, SOURCE)?;
    Ok(InstallReport { path, status })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_creates_updates_and_keeps_backup() {
        let lib = tempfile::tempdir().unwrap();
        let first = install(lib.path()).unwrap();
        assert_eq!(first.status, InstallStatus::Created);
        assert_eq!(fs::read_to_string(&first.path).unwrap(), SOURCE);

        assert_eq!(install(lib.path()).unwrap().status, InstallStatus::Unchanged);

        fs::write(&first.path, "old").unwrap();
        let third = install(lib.path()).unwrap();
        let InstallStatus::Updated { backup } = third.status else { panic!("expected update") };
        assert_eq!(fs::read_to_string(backup).unwrap(), "old");
        assert_eq!(fs::read_to_string(&third.path).unwrap(), SOURCE);
    }

    #[test]
    fn install_refuses_missing_library() {
        let lib = tempfile::tempdir().unwrap();
        let err = install(&lib.path().join("nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn embedded_script_matches_protocol_constants() {
        let protocol = format!("PROTOCOL_VERSION = {}", live_mcp_protocol::PROTOCOL_VERSION);
        let port = format!("DEFAULT_PORT = {}", live_mcp_protocol::DEFAULT_PORT);
        assert!(SOURCE.contains(&protocol), "script and crate disagree on {protocol}");
        assert!(SOURCE.contains(&port), "script and crate disagree on {port}");
    }
}
