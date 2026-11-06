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

The Live API is only safe on Live's main thread, and Live only runs Python
code there during its UI ticks. Measured on Live 12.4.16, `update_display()`
runs at about 9.5 Hz. Python threads exist inside Live, but a non-main thread
only gets the interpreter lock while the main thread is executing Python. In
practice a background socket thread waits up to one tick at *every* blocking
step: a first version built that way took about 1 s to connect and about
300 ms per request.

So the script starts **no threads**. Its sockets are non-blocking, and every
`update_display()` tick it:

1. accepts pending connections;
2. reads whatever bytes have arrived;
3. answers complete request lines round-robin across clients until the tick's
   50 ms budget is used (the first request of a tick always runs);
4. writes as much buffered output as the sockets accept;
5. if it answered anything, lingers: it waits up to 5 ms (with `select`) for
   follow-up requests and answers them in the same tick. It repeats this until
   no request arrives within 5 ms or the 50 ms budget is used.

Consequences:

- The first request after an idle period waits for the next tick: 0–105 ms at
  Live 12.4's roughly 9.5 Hz. Follow-ups that arrive within 5 ms of a response,
  pipelined requests and `batch` ops are answered in the same tick. Measured on
  Live 12.4.16: about 0.1 ms per back-to-back `get`, 1 ms for 50 pipelined gets.
  `crates/bridge/examples/latency.rs` reproduces these numbers.
- Lingering costs Live's main thread at most 5 ms idle time per tick, and only
  in ticks that answered a request. `info.linger` reports whether it is
  available. Without `select`, each request waits for its own tick.
- Requests from all connections are serialised, and all of them, including
  `hello` and `status`, run on the main thread.
- A long-running call blocks Live's UI, and every other client, until it returns.

Timeouts are purely client-side. A request that times out may still be
executed later (when Live's main thread gets to it) or may already have run.
Its outcome is unknown, so clients must not retry automatically: a retry could
duplicate tracks or clips. Check the state instead. The client discards a late
response to a request it has given up on.

## Targets

Methods that operate on an object take a `target`:

- a path string from a root, in the style of Max for Live paths:
  `"live_set tracks 0 clip_slots 2 clip"`. The roots are `live_set` (alias
  `song`) and `live_app` (alias `app`). Integer tokens index into the
  preceding sequence, and other tokens are attribute names;
- `{"id": N}`: an object returned earlier as `{"$obj": N, ...}`;
- `{"id": N, "path": "clip_slots 0"}`: a path relative to that object;
- inside a `batch` only, `{"result": K}` or `{"result": K, "path": "..."}`: the Live object
  returned by op K of the same batch (a `set` result counts as its read-back value). This
  lets one batch create something and configure it, as a single undo step. Rust clients
  write it as `@K` in the compact text form.

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

If Live rejects a `set` value's type (a `TypeError`, such as Boost.Python's
`ArgumentError`) and the value is a string that is a JSON number literal, such
as `"1.0"` or `"0.50"`, the script retries it once as that number. Some MCP
clients send numbers that way when the tool schema leaves `value` untyped. A
property that takes a string, such as a track name, keeps the string.

## Methods

| Method | Params | Result |
|---|---|---|
| `hello` | `token`, `protocol`, `client` | `{script_version, protocol, code_hash}` |
| `status` | | `{ticks, uptime, tick_rate, clients, pending_requests}` |
| `info` | | script, Python and Live versions, platform |
| `get` | `target`, `props?`, `fields?`, `offset?`, `limit?` | the encoded target, or `{prop: value}` when `props` is given |
| `set` | `target`, `prop`, `value` | `{"value": <read back after setting>}` |
| `call` | `target`, `method`, `args?`, `kwargs?`, `fields?` | the encoded return value |
| `describe` | `target`, `values?` | `{type, module, properties, methods, types, observable, values?}`. `types` lists nested classes and enums such as `Track.View` |
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
| -32003 | unknown or expired object id |
| -32004 | unavailable: too many clients, or the script is shutting down |

## Deliberately absent

- **No `eval`/`exec`.** Anything the generic methods can't express should be
  added as a method here, on purpose.
- No listeners or events yet. `observe` and `events` are planned, based on
  `add_<prop>_listener`, which `describe` already lists as `observable`.
