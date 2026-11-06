//! The LiveMCP Ear plugin bundle embedded at build time (see `build.rs`), and installing it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct Bundle {
    /// The bundle folder's name, e.g. "LiveMCP Ear.vst3".
    pub name: &'static str,
    pub files: &'static [EmbeddedFile],
}

pub struct EmbeddedFile {
    /// Relative to the bundle folder, with `/` separators.
    pub path: &'static str,
    pub bytes: &'static [u8],
    pub executable: bool,
}

// Defines `EMBEDDED: Option<Bundle>`.
include!(concat!(env!("OUT_DIR"), "/ear_bundle.rs"));

/// The per-user VST3 folder Live scans when "Use VST3 Plug-In System Folders" is on.
pub fn default_vst3_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(live_mcp_bridge::paths::home_dir()?.join("Library/Audio/Plug-Ins/VST3"))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStatus {
    Created,
    Unchanged,
    Updated,
}

#[derive(Debug)]
pub struct InstallReport {
    pub path: PathBuf,
    pub status: InstallStatus,
}

/// Installs `bundle` as `<vst3_dir>/<bundle.name>`, replacing any other version.
pub fn install(bundle: &Bundle, vst3_dir: &Path) -> io::Result<InstallReport> {
    let dest = vst3_dir.join(bundle.name);
    if is_current(bundle, &dest)? {
        return Ok(InstallReport { path: dest, status: InstallStatus::Unchanged });
    }
    fs::create_dir_all(vst3_dir)?;
    // Write a complete new bundle next to the old one and swap them with renames. Writing over
    // the old files instead could change a binary that Live has loaded, and macOS kills a process
    // whose signed code changes under it. The temporary names don't end in .vst3, so a plugin
    // scan never picks them up.
    let pid = std::process::id();
    let staging = vst3_dir.join(format!(".{}.new-{pid}", bundle.name));
    remove_if_present(&staging)?;
    if let Err(e) = write_bundle(bundle, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }
    let status = if fs::symlink_metadata(&dest).is_ok() {
        let old = vst3_dir.join(format!(".{}.old-{pid}", bundle.name));
        remove_if_present(&old)?;
        fs::rename(&dest, &old)?;
        if let Err(e) = fs::rename(&staging, &dest) {
            let _ = fs::rename(&old, &dest);
            let _ = fs::remove_dir_all(&staging);
            return Err(e);
        }
        // The old bundle is out of the way; failing to delete it leaves only a hidden folder.
        let _ = fs::remove_dir_all(&old);
        InstallStatus::Updated
    } else {
        fs::rename(&staging, &dest)?;
        InstallStatus::Created
    };
    Ok(InstallReport { path: dest, status })
}

/// Whether `dest` holds exactly the files of `bundle`.
pub fn is_current(bundle: &Bundle, dest: &Path) -> io::Result<bool> {
    if !dest.is_dir() {
        return Ok(false);
    }
    let mut installed = Vec::new();
    list_files(dest, dest, &mut installed)?;
    if installed.len() != bundle.files.len() {
        return Ok(false);
    }
    for file in bundle.files {
        match fs::read(dest.join(file.path)) {
            Ok(bytes) if bytes == file.bytes => {}
            Ok(_) => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

fn write_bundle(bundle: &Bundle, dir: &Path) -> io::Result<()> {
    for file in bundle.files {
        let path = dir.join(file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, file.bytes)?;
        set_mode(&path, file.executable)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, executable: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _executable: bool) -> io::Result<()> {
    Ok(())
}

fn list_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if fs::symlink_metadata(&path)?.is_dir() {
            list_files(root, &path, out)?;
        } else {
            out.push(path.strip_prefix(root).unwrap_or(&path).to_owned());
        }
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE: Bundle = Bundle {
        name: "Test Ear.vst3",
        files: &[
            EmbeddedFile { path: "Contents/Info.plist", bytes: b"<plist/>", executable: false },
            EmbeddedFile { path: "Contents/MacOS/Test Ear", bytes: b"\xcf\xfa\xed\xfe", executable: true },
        ],
    };

    fn leftovers(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> =
            fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn installs_into_a_new_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let vst3 = tmp.path().join("Plug-Ins/VST3");
        let report = install(&BUNDLE, &vst3).unwrap();
        assert_eq!(report.status, InstallStatus::Created);
        assert_eq!(report.path, vst3.join("Test Ear.vst3"));
        assert_eq!(fs::read(report.path.join("Contents/Info.plist")).unwrap(), b"<plist/>");
        assert!(is_current(&BUNDLE, &report.path).unwrap());
        assert_eq!(leftovers(&vst3), ["Test Ear.vst3"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &str| fs::metadata(report.path.join(p)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode("Contents/MacOS/Test Ear"), 0o755);
            assert_eq!(mode("Contents/Info.plist"), 0o644);
        }
    }

    #[test]
    fn leaves_a_current_bundle_alone() {
        let tmp = tempfile::tempdir().unwrap();
        install(&BUNDLE, tmp.path()).unwrap();
        assert_eq!(install(&BUNDLE, tmp.path()).unwrap().status, InstallStatus::Unchanged);
    }

    #[test]
    fn replaces_another_version_completely() {
        let tmp = tempfile::tempdir().unwrap();
        let path = install(&BUNDLE, tmp.path()).unwrap().path;
        fs::write(path.join("Contents/MacOS/Test Ear"), b"old binary").unwrap();
        fs::write(path.join("Contents/stale.txt"), b"from an older build").unwrap();
        assert!(!is_current(&BUNDLE, &path).unwrap());

        assert_eq!(install(&BUNDLE, tmp.path()).unwrap().status, InstallStatus::Updated);
        assert!(is_current(&BUNDLE, &path).unwrap());
        assert!(!path.join("Contents/stale.txt").exists());
        assert_eq!(leftovers(tmp.path()), ["Test Ear.vst3"]);
    }

    #[test]
    fn an_extra_file_makes_a_bundle_outdated() {
        let tmp = tempfile::tempdir().unwrap();
        let path = install(&BUNDLE, tmp.path()).unwrap().path;
        fs::write(path.join("Contents/extra"), b"").unwrap();
        assert!(!is_current(&BUNDLE, &path).unwrap());
        assert!(!is_current(&BUNDLE, &tmp.path().join("missing.vst3")).unwrap());
    }
}
