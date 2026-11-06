//! Embeds the LiveMCP Ear plugin bundle in the binary when `LIVE_MCP_EAR_BUNDLE` names it,
//! so that `live-mcp install` can install the plugin too.
//!
//! `just build` sets it. By hand:
//!   cargo xtask bundle live-mcp-ear --release
//!   LIVE_MCP_EAR_BUNDLE="target/bundled/LiveMCP Ear.vst3" cargo build --release
//! A relative path is taken from the workspace root. Without the variable, the binary has
//! no plugin and `install` says so.

use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const VAR: &str = "LIVE_MCP_EAR_BUNDLE";

fn main() {
    println!("cargo:rerun-if-env-changed={VAR}");
    let code = match env::var(VAR) {
        Ok(path) if !path.is_empty() => embed(&resolve(&path)).unwrap_or_else(|e| panic!("{VAR}={path}: {e}")),
        _ => "pub const EMBEDDED: Option<Bundle> = None;\n".to_owned(),
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("ear_bundle.rs");
    fs::write(out, code).expect("writing ear_bundle.rs");
}

fn resolve(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return path;
    }
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    manifest.join("../..").join(path)
}

fn embed(bundle: &Path) -> Result<String, String> {
    let name = bundle.file_name().and_then(|n| n.to_str()).ok_or("not a bundle path")?;
    if !name.ends_with(".vst3") {
        return Err("expected a .vst3 bundle".into());
    }
    if !bundle.join("Contents").is_dir() {
        return Err("not a plugin bundle (no Contents folder); build it with `cargo xtask bundle`".into());
    }
    let mut files = Vec::new();
    walk(bundle, bundle, &mut files)?;
    let mut code = format!("pub const EMBEDDED: Option<Bundle> = Some(Bundle {{\n    name: {name:?},\n    files: &[\n");
    for (relative, absolute, executable) in &files {
        writeln!(
            code,
            "        EmbeddedFile {{ path: {relative:?}, bytes: include_bytes!({absolute:?}), executable: {executable} }},"
        )
        .unwrap();
    }
    code.push_str("    ],\n});\n");
    Ok(code)
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, String, bool)>) -> Result<(), String> {
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries = fs::read_dir(dir)
        .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in &entries {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            walk(root, &path, files)?;
        } else if meta.is_file() {
            println!("cargo:rerun-if-changed={}", path.display());
            let relative = path.strip_prefix(root).unwrap().to_str().ok_or("non-UTF-8 file name")?;
            let absolute = path.canonicalize().map_err(|e| e.to_string())?;
            files.push((
                relative.replace('\\', "/"),
                absolute.to_str().ok_or("non-UTF-8 path")?.to_owned(),
                executable(&meta),
            ));
        } else {
            return Err(format!("{}: symlinks are not supported in the bundle", path.display()));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_meta: &fs::Metadata) -> bool {
    false
}
