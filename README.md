# live-mcp

[![CI](https://github.com/buntec/live-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/buntec/live-mcp/actions/workflows/ci.yml)

An MCP server for Ableton Live, written in Rust. An agent can read and edit
your set, and hear it through the LiveMCP Ear plugin: loudness, tonal balance,
stereo, masking between tracks, key, chords, bass line and rhythm.

The only code that runs inside Live is a small, generic Python Remote Script
(`remote_script/LiveMCP`). It exposes Live's object model over a local,
token-authenticated socket. Everything else is Rust.

> **Status: early, but working.** Tested on Live 12.4 (macOS, arm64).

## Install

Requires Ableton Live 12 on macOS (Apple Silicon or Intel).

```sh
mkdir -p ~/.local/bin
curl -fsSL https://github.com/buntec/live-mcp/releases/latest/download/live-mcp-macos-universal.tar.gz \
  | tar -xz -C ~/.local/bin
~/.local/bin/live-mcp install       # or: install --user-library "/path/to/User Library"
```

The binary is signed and notarized by Apple. `install` puts the Remote Script
in Live's User Library and the LiveMCP Ear plugin in
`~/Library/Audio/Plug-Ins/VST3` (`--vst3-dir` to change it). Then:

1. Quit and restart Live if it's running.
2. In Settings → Link, Tempo & MIDI, pick **LiveMCP** as a Control Surface, with
   Input and Output set to None.

To update, run the same commands again and **restart Live**: until then it keeps
the old script and plugin loaded. `live-mcp status` reports whether both are
current. To build from source instead, see [Development](docs/development.md).

## Connect an MCP client

`live-mcp serve` speaks MCP over stdin/stdout:

```sh
claude mcp add live -- ~/.local/bin/live-mcp serve   # Claude Code
codex mcp add live -- ~/.local/bin/live-mcp serve    # Codex CLI
```

Other clients take the same command and arguments in their MCP server config.
Each client starts its own server, and only the first one gets the Ear port, so
`listen` and `ears` work in one client at a time.

## Tools

| Tool                                      | What it does                                                                  |
| ----------------------------------------- | ----------------------------------------------------------------------------- |
| `session`                                 | Overview: tempo, transport, tracks with devices, clips and automation, scenes |
| `create_track`                            | New MIDI, audio or return track                                               |
| `write_clip`, `read_clip`                 | MIDI notes in Session slots or on the Arrangement timeline                    |
| `browser`, `load_device`                  | List or search Live's browser, load an item onto a track                      |
| `device_parameters`, `set_parameter`      | Read and set device parameters by index or name, including inside Racks       |
| `transport`                               | Play/stop, tempo, position, loop, fire clips and scenes                       |
| `listen`, `ears`                          | Play bars and get per-bar measurements from LiveMCP Ear plugins               |
| `analyze_file`                            | The same measurements for an audio file, such as a reference track            |
| `compare`, `snapshots`                    | How two results differ: before/after an edit, or your mix against a reference |
| `observe`, `events`, `unobserve`          | Watch properties and get what changed, including changes made by hand in Live |
| `get`, `set`, `call`, `describe`, `batch` | Generic access to the whole Live Object Model                                 |
| `status`                                  | Connection and version check                                                  |

The same commands work from the terminal: see [Command line](docs/cli.md).

## Documentation

- [Listening](docs/listening.md): setting up the Ear, what `listen`, `compare`
  and the masking analysis report, and how Live's transport behaves
- [Command line](docs/cli.md): `live-mcp` without an MCP client
- [Development](docs/development.md): code layout, building from source, tests
- [Bridge protocol](docs/protocol.md): the contract between the Remote Script
  and its clients
- [Hearing harmony and rhythm](docs/harmony.md): design notes for key, chord,
  bass and rhythm detection

## Security

live-mcp gives whatever drives it full control of Live: every connected MCP
client can create, change and delete anything in your set. Live's undo history
is the safety net, so save your set before handing it to an agent. See
[SECURITY.md](SECURITY.md) for details and how to report a vulnerability.

## License

MIT. See [LICENSE](LICENSE).

live-mcp is an independent project, not affiliated with or endorsed by Ableton
AG. Ableton and Live are trademarks of Ableton AG. VST is a registered
trademark of Steinberg Media Technologies GmbH.
