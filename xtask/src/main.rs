//! `cargo xtask bundle live-mcp-ear [--release]` builds the plugin into `target/bundled/`.
//!
//! nice-plug's bundler writes `com.nice-plug.<package>` as the macOS bundle identifier and
//! 1.0.0 as the version, with no setting for either. After it runs, this rewrites both in each
//! bundle's Info.plist (identifier [`BUNDLE_ID_PREFIX`]`<package>`, the workspace version) and
//! signs the bundle again, since the signature covers Info.plist.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const BUNDLE_ID_PREFIX: &str = "io.github.buntec.";
const NICE_PLUG_PREFIX: &str = "<string>com.nice-plug.";
const NICE_PLUG_VERSION: &str = "<string>1.0.0</string>";

fn main() -> nice_plug_xtask::Result<()> {
    let bundling = std::env::args().nth(1).is_some_and(|command| command.starts_with("bundle"));
    nice_plug_xtask::main()?;
    // nice_plug_xtask::main has changed to the workspace root.
    if bundling && cfg!(target_os = "macos") {
        let target = PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| OsString::from("target")));
        fix_bundles(&target.join("bundled"))?;
    }
    Ok(())
}

fn fix_bundles(bundled: &Path) -> io::Result<()> {
    let Ok(entries) = fs::read_dir(bundled) else { return Ok(()) };
    for entry in entries {
        let bundle = entry?.path();
        let plist_path = bundle.join("Contents").join("Info.plist");
        let Ok(plist) = fs::read_to_string(&plist_path) else { continue };
        if !plist.contains(NICE_PLUG_PREFIX) {
            continue; // not a nice-plug bundle, or already fixed
        }
        let version = format!("<string>{}</string>", env!("CARGO_PKG_VERSION"));
        let fixed = plist
            .replace(NICE_PLUG_PREFIX, &format!("<string>{BUNDLE_ID_PREFIX}"))
            .replace(NICE_PLUG_VERSION, &version);
        fs::write(&plist_path, fixed)?;
        let status = Command::new("codesign").args(["--force", "--sign", "-"]).arg(&bundle).status()?;
        if !status.success() {
            return Err(io::Error::other(format!("codesign failed for {}", bundle.display())));
        }
        println!("Set the bundle identifier and version of {}", bundle.display());
    }
    Ok(())
}
