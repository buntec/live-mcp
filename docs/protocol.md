# LiveMCP bridge protocol, v1

This is the contract between the Remote Script running inside Ableton Live
(`remote_script/LiveMCP/__init__.py`) and its Rust clients
(`crates/protocol`, `crates/bridge`). The script is deliberately generic: it
exposes the Live object model (LOM) through a few reflective methods, and all
music-specific logic lives on the Rust side.

## Transport

- TCP on `127.0.0.1` only. The default port is `19418`. `~/.live-mcp/config.json`
  (`{"port": N}`; `0` means any free port) overrides it.
- When the script starts listening, it writes `~/.live-mcp/bridge.json`:
  `{"port", "pid", "script_version", "protocol", "code_hash"}`. It removes the
  file on clean shutdown, so the file can be stale after a crash.
- Framing is **one JSON-RPC 2.0 message per line** (UTF-8, terminated by `\n`).
  A line can be at most 8 MiB. A malformed line gets a parse error, and the
  connection carries on.
- Up to 8 concurrent connections. Requests on one connection are processed in order.

## Authentication

The first request on every connection must be:

```json
{"jsonrpc":"2.0","id":1,"method":"hello","params":{"token":"…","protocol":1,"client":"live-mcp 0.1.0"}}
```

The token is the content of `~/.live-mcp/token`: 64 hex characters, mode
0600. The installer or the script creates it, whichever runs first. The script
re-reads the token on every `hello`. A wrong token, or any other method sent
before `hello`, gets error `-32001` and the connection is closed. A `protocol`
mismatch gets `-32600`, and the connection stays open.

`hello` returns `{"script_version", "protocol", "code_hash"}`. `code_hash` is
the sha256 of the installed script file. The client compares it with the copy
embedded in its binary to detect a stale install.

## Threading model

The Live API is only safe on Live's main thread. The script queues every Live
API request and works through the queue in `update_display()`, which Live calls
on its main thread at its UI refresh rate (assumed to be about 10 Hz; not yet
measured on real Live). Each tick has a 50 ms time budget. So:

- latency is roughly one tick per request, which is the reason `batch` exists;
- requests from all connections are serialised;
- `hello` and `status` run on the socket thread and never touch the Live API,
  so `status` keeps answering while the main thread is blocked.

Every request accepts `params.timeout` (seconds, default 30, max 600). On
expiry the script returns `-32002` with `data.started`:

- `false`: the job never started and never will. It is safe to resend.
- `true`: the job is running, and its outcome is **unknown**. Clients must not
  retry automatically, because a retry could duplicate tracks or clips. Check the
  state instead.

## Targets

Methods that operate on an object take a `target`:

- a path string from a root, in the style of Max for Live paths:
  `"live_set tracks 0 clip_slots 2 clip"`. The roots are `live_set` (alias
  `song`) and `live_app` (alias `app`). Integer tokens index into the
  preceding sequence, and other tokens are attribute names;
- `{"id": N}`: an object returned earlier as `{"$obj": N, ...}`;
- `{"id": N, "path": "clip_slots 0"}`: a path relative to that object.

Attribute and method names must match `[A-Za-z][A-Za-z0-9_]*`, so nothing that
starts with an underscore is reachable. `set` and `call` require the target to
be a Live object, not a plain value such as a string or number.

Object ids come from the LOM object's `_live_ptr` when it has one, so the same
Live object gets the same id. The script keeps the 50 000 most recently
returned objects. An unknown or evicted id gets `-32003`; resolve the object
again by path.

## Value encoding (script → client)

| Python / Live value | JSON |
|---|---|
| `None`, `bool`, `str` | as is |
| `int`, including Live enums | number |
| `float` | number. NaN and ±inf become `null` |
| list, tuple, Live vector | array, or `{"total", "offset", "items"}` when `offset`/`limit` are given |
| dict | object |
| Live object | `{"$obj": id, "type": "Track", "fields": {...}?}` |
| method or other callable | `{"$callable": name}` |

`fields: ["name", ...]` on `get` and `call` reads those properties from every
Live object in the result, one level deep. A field that fails reads as
`{"$error": "..."}`.

## Argument decoding (client → script)

Inside `value`, `args` and `kwargs`:

| JSON | Python |
|---|---|
| `{"$ref": <target>}` | the resolved Live object |
| `{"$tuple": [...]}` | a tuple (many LOM methods reject lists) |
| `{"$new": "Clip.MidiNoteSpecification", "args": [...], "kwargs": {...}}` | an instance of that class. Only names inside the `Live` module are allowed. |
| anything else | the plain JSON value |

## Methods

| Method | Params | Result |
|---|---|---|
| `hello` | `token`, `protocol`, `client` | `{script_version, protocol, code_hash}` |
| `status` | | `{queued, ticks, last_tick_age, busy_method, busy_for}` (socket thread) |
| `info` | | script, Python and Live versions, platform |
| `get` | `target`, `props?`, `fields?`, `offset?`, `limit?` | the encoded target, or `{prop: value}` when `props` is given |
| `set` | `target`, `prop`, `value` | `{"value": <read back after setting>}` |
| `call` | `target`, `method`, `args?`, `kwargs?`, `fields?` | the encoded return value |
| `describe` | `target`, `values?` | `{type, module, properties, methods, observable, values?}` |
| `batch` | `ops: [{method, params}]`, `stop_on_error?` (default true), `undo_step?` | `{"results": [{"ok": true, "result"} \| {"ok": false, "error"}]}` |

`batch` runs all of its ops in one main-thread job, back to back. With
`undo_step: true`, they are wrapped in `song.begin_undo_step()` /
`end_undo_step()` when Live provides those methods. Batches cannot be nested.

Any request can pass `debug: true` to get a Python traceback in `error.data`.

## Errors

| Code | Meaning |
|---|---|
| -32700 | parse error |
| -32600 | invalid request, or protocol version mismatch |
| -32601 | unknown method |
| -32602 | invalid params: bad path, index or name, or a target that is not a Live object |
| -32000 | the Live API raised. `data.type` is the exception class |
| -32001 | unauthorised. The connection is closed |
| -32002 | main-thread timeout. See `data.started` |
| -32003 | unknown or expired object id |
| -32004 | unavailable: too many clients, or the script is shutting down |

## Deliberately absent

- **No `eval`/`exec`.** Anything the generic methods can't express should be
  added as a method here, on purpose.
- No listeners or events yet. `observe` and `events` are planned, based on
  `add_<prop>_listener`, which `describe` already lists as `observable`.
