# live-mcp

An MCP server for Ableton Live, written in Rust.

The only code that runs inside Live is a small, generic Python Remote Script
(`remote_script/LiveMCP`). It exposes Live's object model over a local,
token-authenticated JSON-RPC socket. Everything else is Rust.

> **Status: M2 in progress.** The bridge to Live, the command-line client and
> the MCP server work and have been tested on Live 12.4.16 (macOS). The
> audio-analysis plugin, LiveMCP Ear, builds and loads in Live, but for now it
> only passes audio through. It doesn't measure anything yet.

## Layout

| Path | What |
|---|---|
| `remote_script/LiveMCP/__init__.py` | The Remote Script: standard library only, no music logic |
| `crates/protocol` | Wire types shared by every client |
| `crates/bridge` | Async client, token and endpoint files, embedded script and installer |
| `crates/server` | `live-mcp` binary: the MCP server (`serve`) and CLI commands |
| `crates/ear` | LiveMCP Ear, the VST3 analysis plugin ([nice-plug](https://codeberg.org/RustAudio/nice-plug)) |
| `crates/analysis` | Audio measurements, independent of any plugin framework |
| `xtask` | `cargo xtask bundle`, which builds plugin bundles |
| `docs/protocol.md` | The bridge protocol spec |

## Try it

Requires Ableton Live 12. Live 11 may work, but it hasn't been tested.

```sh
cargo build --release
./target/release/live-mcp install            # or: install --user-library "/path/to/User Library"
```

In Live, open Settings → Link, Tempo & MIDI and pick **LiveMCP** as a Control
Surface, with Input and Output set to None. Then:

```sh
live-mcp status
live-mcp get "live_set tracks" --fields name,mute
live-mcp describe "live_set tracks 0" --values
live-mcp set "live_set" tempo 124
live-mcp call "live_set tracks 0 clip_slots 0" create_clip 4
live-mcp get "#<id> clip_slots 0 clip" --props name,length
```

`set` values and `call` arguments are JSON, so strings need quotes: `'"Bass"'`.

### Use it from an MCP client

`live-mcp serve` speaks MCP over stdin/stdout. For Claude Code:

```sh
claude mcp add live -- /absolute/path/to/target/release/live-mcp serve
```

Other clients take the same command and arguments in their MCP server config.

| Tools | |
|---|---|
| `session` | Overview: tempo, transport, tracks with devices and clips, returns, scenes |
| `create_track` | New MIDI, audio or return track |
| `write_clip`, `read_clip` | MIDI notes in Session slots or on the Arrangement timeline |
| `browser`, `load_device` | List or search Live's browser, load an item onto a track |
| `device_parameters`, `set_parameter` | Read and set device parameters by index or name |
| `transport` | Play/stop, tempo, position, loop, fire clips and scenes |
| `get`, `set`, `call`, `describe`, `batch` | Generic access to the whole Live Object Model |
| `status` | Connection and script version check |

After `live-mcp install` replaces an older version, **quit and restart Live**.
Re-selecting the control surface creates a new instance but keeps the old code.
`live-mcp status` reports `script_current: false` until Live has restarted.

## The plugin

```sh
cargo xtask bundle live-mcp-ear --release      # → target/bundled/LiveMCP Ear.vst3
cp -R "target/bundled/LiveMCP Ear.vst3" ~/Library/Audio/Plug-Ins/VST3/
```

In Live, open Settings → Plug-Ins, turn on **Use VST3 Plug-In System Folders**,
and click **Rescan**. The plugin appears under Plug-Ins → live-mcp → LiveMCP Ear,
and `load_device` can load it with the path `["plugins", "live-mcp", "LiveMCP Ear"]`.

## Tests

```sh
cargo test
```

The end-to-end tests run the real Remote Script on a fake `Live` module
(`crates/bridge/tests/fake_live/`, test-only Python). The fake raises if
anything touches it off the main thread. The tests need `python3` on PATH and
are skipped without it.
