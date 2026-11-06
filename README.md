# live-mcp

An MCP server for Ableton Live, written in Rust.

The only code that runs inside Live is a small, generic Python Remote Script
(`remote_script/LiveMCP`). It exposes Live's object model over a local,
token-authenticated JSON-RPC socket. Everything else is Rust.

> **Status: M2.** The bridge to Live, the command-line client, the MCP server,
> and listening through the LiveMCP Ear plugin all work, and have been tested on
> Live 12.4.16 (macOS, arm64).

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

For Codex CLI:

```sh
codex mcp add live -- /absolute/path/to/target/release/live-mcp serve
```

or, in `~/.codex/config.toml`:

```toml
[mcp_servers.live]
command = "/absolute/path/to/target/release/live-mcp"
args = ["serve"]
```

Other clients take the same command and arguments in their MCP server config.
Each client starts its own server, and only the first one gets the Ear port
(127.0.0.1:19419), so `listen` and `ears` work in one client at a time.

| Tools | |
|---|---|
| `session` | Overview: tempo, transport, tracks with devices and clips, returns, scenes |
| `create_track` | New MIDI, audio or return track |
| `write_clip`, `read_clip` | MIDI notes in Session slots or on the Arrangement timeline |
| `browser`, `load_device` | List or search Live's browser, load an item onto a track |
| `device_parameters`, `set_parameter` | Read and set device parameters by index or name |
| `transport` | Play/stop, tempo, position, loop, fire clips and scenes |
| `listen`, `ears` | Play bars and get per-bar measurements from a LiveMCP Ear plugin |
| `analyze_file` | The same measurements for an audio file (e.g. a reference track), plus gated integrated loudness and loudness range |
| `compare`, `snapshots` | How two results differ (before/after an edit, or your mix against a reference) |
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
After rebuilding the plugin, restart Live: Live keeps the old binary loaded.

### Listening

Put a LiveMCP Ear wherever you want to listen: on the master track to hear the
whole mix, or on a single track. `listen` plays `bars` bars from `start_bar` in
real time and returns, for each bar:
- loudness (LUFS, ITU-R BS.1770);
- peak and RMS (dBFS), and crest factor;
- energy in five bands (dBFS): low <120 Hz, low_mid 120–500 Hz, mid 500 Hz–2 kHz,
  high_mid 2–6 kHz, high >6 kHz;
- stereo correlation and width, and clipped samples.

It also returns a summary of the whole section. `ears` shows which tracks have an
Ear and whether each is connected.

Every `listen` and `analyze_file` result is kept for the server session as a
snapshot (`s1`, `s2`, …). `compare` reports how two of them differ, optionally
for given bar ranges ("my bars 9–16" against "the reference's bars 33–40"):
- loudness, peak, crest (dynamics), width and correlation;
- **tonal balance with levels matched** on the median band, so a change in one
  band shows only in that band;
- plain-language notes on the differences that matter;
- bar-by-bar differences when both selections have the same length.

`listen` with `tracks` (a list, or `["all"]`) listens through several Ears in
one playback. It returns each track's bars (each kept as its own snapshot), plus a
**masking analysis**:
- each track's share of every frequency band;
- the bands that two or more tracks each carry at least a quarter of (−6 dB),
  such as bass and kick both in the low band.

An Ear at the end of a track's device chain hears the signal before the fader,
so levels are corrected by each track's volume, and muted tracks are left out.
Pan and sends are not yet taken into account.

How it works:
- The plugin measures audio on the audio thread without allocating (about
  0.07% of one core). It streams per-bar reports to `live-mcp serve`, which
  listens on 127.0.0.1:19419 and writes the actual port to `~/.live-mcp/ear.json`.
- The plugin's **Tag** parameter is set by the server through the Live API, and
  tells the server which track each plugin instance is on. Leave it alone.

### Live's transport, as measured on 12.4

- `start_playing` always starts at the song start.
- `continue_playing` resumes where playback last *stopped*. A position set while
  stopped is shown, but ignored.
- Setting `current_song_time` while playing jumps there.
- All of these take effect on a later tick: `is_playing` and `current_song_time`
  read stale until then.

So `listen` and `transport` start playback at a position by starting playback,
waiting a tick, then jumping. Up to one tick (about 0.1 s) of audio from the song
start can be heard first.

## Tests

```sh
cargo test
```

`just check` runs everything to check before committing: formatting, clippy,
a syntax check of the Remote Script, and the tests. Run `just` to list the
other recipes (build, install, bundle the Ear, run the CLI).

The end-to-end tests run the real Remote Script on a fake `Live` module
(`crates/bridge/tests/fake_live/`, test-only Python). The fake raises if
anything touches it off the main thread. The tests need `python3` on PATH and
are skipped without it.
