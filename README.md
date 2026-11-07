# live-mcp

[![CI](https://github.com/buntec/live-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/buntec/live-mcp/actions/workflows/ci.yml)

An MCP server for Ableton Live, written in Rust.

The only code that runs inside Live is a small, generic Python Remote Script
(`remote_script/LiveMCP`). It exposes Live's object model over a local,
token-authenticated JSON-RPC socket. Everything else is Rust.

> **Status: early, but working.** The bridge to Live, the command-line client,
> the MCP server, listening through the LiveMCP Ear plugin, reference-file
> analysis, snapshot comparison and multi-track masking analysis all work, and
> have been tested on Live 12.4.16 (macOS, arm64). See [Not yet](#not-yet) for
> what's missing.

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

## Install

Requires Ableton Live 12 on macOS (Apple Silicon or Intel). Live 11 may work,
but it hasn't been tested.

```sh
mkdir -p ~/.local/bin
curl -fsSL https://github.com/buntec/live-mcp/releases/latest/download/live-mcp-macos-universal.tar.gz \
  | tar -xz -C ~/.local/bin
~/.local/bin/live-mcp install       # or: install --user-library "/path/to/User Library"
```

The binary is signed and notarized by Apple. You can also download the archive
from the [Releases](https://github.com/buntec/live-mcp/releases) page and put
`live-mcp` wherever you like.

`install` puts the Remote Script in Live's User Library and the LiveMCP Ear
plugin in `~/Library/Audio/Plug-Ins/VST3` (`--vst3-dir` to change it). Then, in
Live:
1. If Live is running, quit and restart it.
2. Open Settings → Link, Tempo & MIDI and pick **LiveMCP** as a Control Surface,
   with Input and Output set to None.

To update, run the same commands again and restart Live.

### Connect an MCP client

`live-mcp serve` speaks MCP over stdin/stdout. For Claude Code:

```sh
claude mcp add live -- ~/.local/bin/live-mcp serve
```

For Codex CLI:

```sh
codex mcp add live -- ~/.local/bin/live-mcp serve
```

or, in `~/.codex/config.toml` (with your home folder in the path):

```toml
[mcp_servers.live]
command = "/Users/you/.local/bin/live-mcp"
args = ["serve"]
```

Other clients take the same command and arguments in their MCP server config.
Each client starts its own server, and only the first one gets the Ear port
(127.0.0.1:19419), so `listen` and `ears` work in one client at a time.

### Use it from the terminal

With `~/.local/bin` on your `PATH`:

```sh
live-mcp status
live-mcp get "live_set tracks" --fields name,mute
live-mcp describe "live_set tracks 0" --values
live-mcp set "live_set" tempo 124
live-mcp call "live_set tracks 0 clip_slots 0" create_clip 4
live-mcp get "#<id> clip_slots 0 clip" --props name,length
live-mcp watch "live_set" tempo is_playing   # print changes until Ctrl-C
```

`set` values and `call` arguments are JSON, so strings need quotes: `'"Bass"'`.

### Build from source

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

### Tools

| Tool | What it does |
|---|---|
| `session` | Overview: tempo, transport, tracks with devices and clips, returns, scenes |
| `create_track` | New MIDI, audio or return track |
| `write_clip`, `read_clip` | MIDI notes in Session slots or on the Arrangement timeline |
| `browser`, `load_device` | List or search Live's browser, load an item onto a track |
| `device_parameters`, `set_parameter` | Read and set device parameters by index or name, including devices inside Racks |
| `transport` | Play/stop, tempo, position, loop, fire clips and scenes |
| `listen`, `ears` | Play bars and get per-bar measurements from a LiveMCP Ear plugin |
| `analyze_file` | The same measurements for an audio file (e.g. a reference track), plus gated integrated loudness and loudness range |
| `compare`, `snapshots` | How two results differ (before/after an edit, or your mix against a reference) |
| `observe`, `events`, `unobserve` | Watch properties (tempo, transport, a track's mute or name, ...) and get what changed since the last call, including changes made by hand in Live |
| `get`, `set`, `call`, `describe`, `batch` | Generic access to the whole Live Object Model |
| `status` | Connection and script version check |

After `live-mcp install` replaces an older version, **quit and restart Live**.
Re-selecting the control surface creates a new instance but keeps the old code.
`live-mcp status` reports `script_current: false` until Live has restarted, and
`ear_plugin` says whether the installed plugin matches the one in the binary.

## The plugin

`live-mcp install` installs the LiveMCP Ear (see [Install](#install)). If Live
doesn't list it, open Settings → Plug-Ins, turn on **Use VST3 Plug-In System
Folders**, and click **Rescan**. The plugin appears under Plug-Ins → live-mcp → LiveMCP Ear,
and `load_device` can load it with the path `["plugins", "live-mcp", "LiveMCP Ear"]`.
After installing a new version of the plugin, restart Live: Live keeps the old binary loaded.

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
so each track's level is corrected by its volume and by the volume of every track
its audio passes through on the way to Main: its groups, or a track it is routed
to. A track routed straight to Main skips its group's fader. Tracks are left out,
each with the reason, when they or a track they play through are muted or silenced
by solo (as Live reports it, whatever the solo mode), or when their output does not
reach Main.

No signal is counted twice. A group track's Ear gives way to its tracks' Ears when
they cover everything audible in the group; otherwise the group's Ear counts, and
the Ears of tracks playing through it do not. An ordinary track that others are
routed into always counts in place of theirs, since it can play clips of its own.

Sends reach the mix through return tracks: put an Ear on a return to count its
reverb or delay as a track of its own. Pan is not taken into account: bands are
measured on the mid signal, as if every track were centred.

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

## Security

live-mcp gives whatever drives it full control of Live:
- Anything that holds the token in `~/.live-mcp/token` (mode 0600) can use the
  whole Live Object Model through the Remote Script: create, change and delete
  tracks, clips and devices, and call any public Live API method. Every MCP
  client you connect `live-mcp serve` to has that access. Live's undo history
  is the safety net, so save your set before handing it to an agent.
- `analyze_file` reads any audio file your user account can read.
- The Remote Script (127.0.0.1:19418) and the Ear listener (127.0.0.1:19419)
  accept connections from this machine only. On a machine shared with other
  users, a process that holds one of those ports while Live or the server isn't
  running would receive the token from a client that connects to it.

To report a vulnerability, use **Report a vulnerability** on the repository's
Security tab rather than a public issue.

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

## Not yet

- **Pan in masking.** Tracks panned apart mask each other less than the analysis
  reports.
- **Other platforms and versions.** Only tested on macOS (arm64) with Live 12.4.

## License

MIT. See [LICENSE](LICENSE).

live-mcp is an independent project, not affiliated with or endorsed by Ableton
AG. Ableton and Live are trademarks of Ableton AG. VST is a registered
trademark of Steinberg Media Technologies GmbH.
