# Development

## Layout

| Path | What |
|---|---|
| `remote_script/LiveMCP/__init__.py` | The Remote Script: standard library only, no music logic |
| `crates/protocol` | Wire types shared by every client |
| `crates/bridge` | Async client, token and endpoint files, embedded script and installer |
| `crates/server` | `live-mcp` binary: the MCP server (`serve`) and CLI commands; embeds the Ear plugin when built with `just build` |
| `crates/ear` | LiveMCP Ear, the VST3 analysis plugin ([nice-plug](https://codeberg.org/RustAudio/nice-plug)) |
| `crates/analysis` | Audio measurements, independent of any plugin framework |
| `xtask` | `cargo xtask bundle`, which builds plugin bundles |
| `scripts/macos-release.sh` | Builds the signed, notarized universal macOS release |
| `docs/protocol.md` | The bridge protocol spec |

## Build from source

```sh
just build                                   # the live-mcp binary, with the LiveMCP Ear plugin inside
./target/release/live-mcp install
```

Without [just](https://github.com/casey/just), build the plugin first and point
the build at it:

```sh
cargo xtask bundle live-mcp-ear --release
LIVE_MCP_EAR_BUNDLE="target/bundled/LiveMCP Ear.vst3" cargo build --release
```

A plain `cargo build` works too, but the binary won't include the plugin. Use
the absolute path of `target/release/live-mcp` in your MCP client's config.

## Tests

`just check` runs everything to check before committing: formatting, clippy, a
syntax check of the Remote Script, and the tests (`cargo test`). Run `just` to
list the other recipes.

The end-to-end tests run the real Remote Script on a fake `Live` module
(`crates/bridge/tests/fake_live/`, test-only Python). The fake raises if
anything touches it off the main thread. The tests need `python3` on `PATH` and
are skipped without it.
