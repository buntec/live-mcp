# live-mcp

An MCP server for Ableton Live, written in Rust.

The only code that runs inside Live is a small, generic Python Remote Script
(`remote_script/LiveMCP`). It exposes Live's object model over a local,
token-authenticated JSON-RPC socket. Everything else is Rust.

> **Status: M0.** The bridge to Live and a command-line client work. So far
> they've only been tested against a fake Live API, not against a real Live.
> The MCP server (M1) and the audio-analysis plugin (M2) haven't been written yet.

## Layout

| Path | What |
|---|---|
| `remote_script/LiveMCP/__init__.py` | The Remote Script: standard library only, no music logic |
| `crates/protocol` | Wire types shared by every client |
| `crates/bridge` | Async client, token and endpoint files, embedded script and installer |
| `crates/server` | `live-mcp` binary (CLI now, MCP server in M1) |
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

## Tests

```sh
cargo test
```

The end-to-end tests run the real Remote Script on a fake `Live` module
(`crates/bridge/tests/fake_live/`, test-only Python). The fake raises if
anything touches it off the main thread. The tests need `python3` on PATH and
are skipped without it.
