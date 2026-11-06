"""LiveMCP Remote Script: a generic JSON-RPC bridge to Ableton Live's object model.

This is the only Python in live-mcp, and it is deliberately dumb. It knows
nothing about tracks, clips or devices. It resolves object paths, reads and
writes properties and calls methods. All domain logic lives in the Rust client.

Wire protocol v1 (full spec: docs/protocol.md in the live-mcp repository):
  * newline-delimited JSON-RPC 2.0 over TCP, bound to 127.0.0.1 only;
  * the first request on a connection must be `hello` with the token stored in
    ~/.live-mcp/token;
  * there are no threads. Sockets are non-blocking and serviced from
    update_display(), which Live calls on its main thread at its UI tick rate
    (about 9.5 Hz measured on Live 12.4). Inside Live, Python threads only run
    while the main thread is executing Python, so a socket thread waits up to
    a tick at every blocking step. Doing all the I/O inside the tick costs at
    most one tick per request, and every Live API call stays on the main thread.

Standard library only: Live's embedded interpreter cannot install packages.
"""

import binascii
import collections
import hashlib
import hmac
import json
import math
import os
import re
import socket
import sys
import time
import traceback

try:
    import select
except ImportError:  # without it, each request just waits for the next tick
    select = None

import Live
from _Framework.ControlSurface import ControlSurface

SCRIPT_VERSION = "0.3.0"
PROTOCOL_VERSION = 1

HOST = "127.0.0.1"
DEFAULT_PORT = 19418
MAX_LINE_BYTES = 8 * 1024 * 1024
MAX_OUTPUT_BYTES = 64 * 1024 * 1024
MAX_CLIENTS = 8
RECV_CHUNK = 256 * 1024
TICK_BUDGET_SECONDS = 0.05
# How long to wait for a follow-up request after answering one, within the tick budget.
LINGER_SECONDS = 0.005
MAX_REGISTRY = 50000

ERR_PARSE = -32700
ERR_INVALID_REQUEST = -32600
ERR_METHOD_NOT_FOUND = -32601
ERR_INVALID_PARAMS = -32602
ERR_LIVE = -32000
ERR_UNAUTHORIZED = -32001
ERR_STALE_REF = -32003
ERR_UNAVAILABLE = -32004

LIVE_API_METHODS = ("info", "get", "set", "call", "describe", "batch")

# Attribute and method names must look like plain identifiers. Rejecting a
# leading underscore keeps reflection from reaching Python internals such as
# __class__ or __globals__, so `call` stays limited to the public Live API.
_NAME_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_]*$")

# `set` and `call` only target Live objects, never plain Python values.
_NOT_LIVE_OBJECTS = (bool, int, float, complex, str, bytes, bytearray,
                     list, tuple, dict, set, frozenset, type)

# A JSON number literal. Some MCP clients send a number as a string when the
# tool schema leaves `value` untyped and the number does not survive a
# parse/print round trip ("1.0", "0.50"). `set` retries such a string as a number.
_JSON_NUMBER_RE = re.compile(r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?$")


def create_instance(c_instance):
    return LiveMCP(c_instance)


class RpcError(Exception):
    def __init__(self, code, message, data=None):
        Exception.__init__(self, message)
        self.code = code
        self.message = message
        self.data = data

    def to_dict(self):
        error = {"code": self.code, "message": self.message}
        if self.data is not None:
            error["data"] = self.data
        return error


class _Client(object):
    """One connection: byte buffers in and out, plus its auth state."""

    __slots__ = ("sock", "inbuf", "outbuf", "authed", "eof", "closing")

    def __init__(self, sock):
        self.sock = sock
        self.inbuf = bytearray()
        self.outbuf = bytearray()
        self.authed = False
        self.eof = False      # peer closed its side; finish buffered requests, then drop
        self.closing = False  # we decided to close; flush the output, then drop

    def next_line(self):
        while True:
            end = self.inbuf.find(b"\n")
            if end < 0:
                return None
            line = bytes(self.inbuf[:end]).strip()
            del self.inbuf[:end + 1]
            if line:
                return line

    def has_line(self):
        return b"\n" in self.inbuf


def _state_dir():
    # The environment override exists for tests; Live itself never sets it.
    return os.environ.get("LIVE_MCP_STATE_DIR") or os.path.join(
        os.path.expanduser("~"), ".live-mcp")


def _read_json(path):
    try:
        with open(path, "r") as handle:
            return json.load(handle)
    except (IOError, OSError, ValueError):
        return None


def _ensure_token(state_dir):
    """Return the shared token, creating it (mode 0600) if it does not exist."""
    path = os.path.join(state_dir, "token")
    try:
        with open(path, "r") as handle:
            token = handle.read().strip()
        if token:
            return token
    except (IOError, OSError):
        pass
    if not os.path.isdir(state_dir):
        os.makedirs(state_dir)
    token = binascii.hexlify(os.urandom(32)).decode("ascii")
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as handle:
        handle.write(token + "\n")
    return token


def _source_hash():
    """sha256 of this file, so the client can detect a stale installed script."""
    try:
        with open(__file__, "rb") as handle:
            return hashlib.sha256(handle.read()).hexdigest()
    except (IOError, OSError, NameError):
        return "unknown"


# Hash the file once, when the module is imported, so it describes the code that is
# actually running. Re-selecting the control surface in Live creates a new instance
# but does not re-import the module, so a hash computed later could describe a newer
# file on disk than the code in memory.
_CODE_HASH = _source_hash()


def _check_name(name):
    if not isinstance(name, str) or not _NAME_RE.match(name):
        raise RpcError(ERR_INVALID_PARAMS, "invalid attribute name: %r" % (name,))
    return name


def _name_list(value, what):
    if value is None:
        return None
    if not isinstance(value, list):
        raise RpcError(ERR_INVALID_PARAMS, "%s must be a list of names" % what)
    return [_check_name(name) for name in value]


def _is_sequence(value):
    if isinstance(value, (list, tuple)):
        return True
    if isinstance(value, (str, bytes, bytearray, dict)):
        return False
    return hasattr(value, "__len__") and hasattr(value, "__iter__")


def _number_from_text(value):
    """The number a JSON number literal string spells, else None."""
    if not isinstance(value, str) or not _JSON_NUMBER_RE.match(value):
        return None
    if "." in value or "e" in value or "E" in value:
        return float(value)
    return int(value)


def _ok(req_id, result):
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def _err(req_id, code, message, data=None):
    return {"jsonrpc": "2.0", "id": req_id,
            "error": RpcError(code, message, data).to_dict()}


def _dumps(payload):
    try:
        text = json.dumps(payload, separators=(",", ":"), allow_nan=False)
    except (TypeError, ValueError) as exc:
        text = json.dumps(_err(payload.get("id"), ERR_LIVE,
                               "result is not JSON-serialisable: %s" % exc))
    return (text + "\n").encode("utf-8")


class LiveMCP(ControlSurface):

    def __init__(self, c_instance):
        ControlSurface.__init__(self, c_instance)
        self._state_dir = _state_dir()
        self._code_hash = _CODE_HASH
        self._registry = collections.OrderedDict()
        self._batch_results = None  # results so far while a batch runs
        self._server = None
        self._port = None
        self._clients = []
        self._started = time.time()
        self._ticks = 0
        try:
            _ensure_token(self._state_dir)
            self._start_server()
            self._log("listening on %s:%d (script %s, protocol %d)"
                      % (HOST, self._port, SCRIPT_VERSION, PROTOCOL_VERSION))
            self.show_message("LiveMCP: listening on port %d" % self._port)
        except Exception as exc:
            self._log("startup failed: %s\n%s" % (exc, traceback.format_exc()))
            self.show_message("LiveMCP: startup failed - %s" % exc)

    # ----------------------------------------------------------------- lifecycle

    def disconnect(self):
        for client in list(self._clients):
            self._drop(client)
        if self._server is not None:
            try:
                self._server.close()
            except Exception:
                pass
            self._server = None
        self._remove_endpoint()
        ControlSurface.disconnect(self)

    def update_display(self):
        ControlSurface.update_display(self)
        self._ticks += 1
        if self._server is None:
            return
        try:
            self._service()
        except Exception as exc:
            self._log("service error: %s\n%s" % (exc, traceback.format_exc()))

    def _log(self, message):
        self.log_message("LiveMCP: " + message)

    # ---------------------------------------------------------------- sockets

    def _start_server(self):
        config = _read_json(os.path.join(self._state_dir, "config.json")) or {}
        port = int(config.get("port", DEFAULT_PORT))
        server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        if not sys.platform.startswith("win"):
            # On Windows SO_REUSEADDR would let another process steal the port.
            server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        server.bind((HOST, port))
        server.listen(MAX_CLIENTS)
        server.setblocking(False)
        self._server = server
        self._port = server.getsockname()[1]
        self._write_endpoint()

    def _endpoint_path(self):
        return os.path.join(self._state_dir, "bridge.json")

    def _write_endpoint(self):
        endpoint = {
            "port": self._port,
            "pid": os.getpid(),
            "script_version": SCRIPT_VERSION,
            "protocol": PROTOCOL_VERSION,
            "code_hash": self._code_hash,
        }
        path = self._endpoint_path()
        tmp = path + ".tmp"
        with open(tmp, "w") as handle:
            json.dump(endpoint, handle)
        os.replace(tmp, path)

    def _remove_endpoint(self):
        path = self._endpoint_path()
        current = _read_json(path) or {}
        if current.get("pid") == os.getpid() and current.get("port") == self._port:
            try:
                os.remove(path)
            except OSError:
                pass

    def _service(self):
        """One tick: accept, read, answer complete requests within budget, write.

        After answering, linger briefly for follow-up requests: a client that
        sends its next request as soon as it has a response would otherwise
        wait a whole tick (~100 ms) per request.
        """
        deadline = time.time() + TICK_BUDGET_SECONDS
        self._accept_pending()
        for client in list(self._clients):
            self._read(client)
        answered = self._answer(deadline)
        self._flush_all()
        linger_until = min(time.time() + LINGER_SECONDS, deadline)
        while answered and select is not None:
            remaining = linger_until - time.time()
            socks = [c.sock for c in self._clients if not (c.closing or c.eof)]
            if remaining <= 0 or not socks:
                break
            try:
                readable = select.select(socks, [], [], remaining)[0]
            except (OSError, ValueError):
                break
            if not readable:
                break
            for client in list(self._clients):
                if client.sock in readable:
                    self._read(client)
            answered = self._answer(deadline)
            self._flush_all()
            linger_until = min(time.time() + LINGER_SECONDS, deadline)
        for client in list(self._clients):
            done_reading = client.eof and not client.has_line()
            if (client.closing or done_reading) and not client.outbuf:
                self._drop(client)

    def _answer(self, deadline):
        """Answer buffered requests round-robin, one per client per pass, so one
        busy client cannot starve the others. Returns how many were answered."""
        answered = 0
        while True:
            progressed = False
            for client in list(self._clients):
                if client.closing:
                    continue
                line = client.next_line()
                if line is None:
                    continue
                progressed = True
                answered += 1
                self._handle(client, line)
                if time.time() >= deadline:
                    return answered
            if not progressed:
                return answered

    def _flush_all(self):
        for client in list(self._clients):
            self._flush(client)

    def _accept_pending(self):
        while True:
            try:
                conn, _addr = self._server.accept()
            except (BlockingIOError, InterruptedError):
                return
            except OSError as exc:
                self._log("accept failed: %s" % exc)
                return
            conn.setblocking(False)
            if len(self._clients) >= MAX_CLIENTS:
                try:
                    conn.send(_dumps(_err(None, ERR_UNAVAILABLE, "too many clients")))
                except OSError:
                    pass
                conn.close()
                continue
            try:
                conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            except OSError:
                pass
            self._clients.append(_Client(conn))

    def _read(self, client):
        while not client.eof and len(client.inbuf) < MAX_LINE_BYTES:
            try:
                data = client.sock.recv(RECV_CHUNK)
            except (BlockingIOError, InterruptedError):
                break
            except OSError:
                self._drop(client)
                return
            if not data:
                client.eof = True
                break
            client.inbuf += data
        if len(client.inbuf) >= MAX_LINE_BYTES and not client.has_line():
            client.inbuf = bytearray()
            self._queue(client, _err(None, ERR_INVALID_REQUEST, "request line too long"))
            client.closing = True

    def _flush(self, client):
        while client.outbuf:
            try:
                sent = client.sock.send(client.outbuf)
            except (BlockingIOError, InterruptedError):
                return
            except OSError:
                self._drop(client)
                return
            del client.outbuf[:sent]

    def _queue(self, client, payload):
        client.outbuf += _dumps(payload)
        if len(client.outbuf) > MAX_OUTPUT_BYTES:
            self._log("dropping a client that is not reading its responses")
            self._drop(client)

    def _drop(self, client):
        if client in self._clients:
            self._clients.remove(client)
        try:
            client.sock.close()
        except Exception:
            pass

    # --------------------------------------------------------------- requests

    def _handle(self, client, line):
        try:
            request = json.loads(line.decode("utf-8"))
        except (ValueError, UnicodeDecodeError) as exc:
            self._queue(client, _err(None, ERR_PARSE, "parse error: %s" % exc))
            return
        if not isinstance(request, dict) or not isinstance(request.get("method"), str):
            req_id = request.get("id") if isinstance(request, dict) else None
            self._queue(client, _err(req_id, ERR_INVALID_REQUEST, "invalid request"))
            return
        req_id = request.get("id")
        method = request["method"]
        params = request.get("params")
        if params is None:
            params = {}
        try:
            if not isinstance(params, dict):
                raise RpcError(ERR_INVALID_PARAMS, "params must be an object")
            if method == "hello":
                result = self._hello(params)
                client.authed = True
            elif not client.authed:
                raise RpcError(ERR_UNAUTHORIZED, "send hello with a valid token first")
            elif method == "status":
                result = self._status()
            elif method in LIVE_API_METHODS:
                result = self._execute(method, params, False)
            else:
                raise RpcError(ERR_METHOD_NOT_FOUND, "unknown method: %s" % method)
            self._queue(client, _ok(req_id, result))
        except RpcError as exc:
            self._queue(client, {"jsonrpc": "2.0", "id": req_id, "error": exc.to_dict()})
            if exc.code == ERR_UNAUTHORIZED:
                client.closing = True
        except Exception as exc:
            self._queue(client, {"jsonrpc": "2.0", "id": req_id,
                                 "error": self._live_error(exc, params)})

    def _hello(self, params):
        token = params.get("token")
        expected = _ensure_token(self._state_dir)  # re-read so rotation works
        if not isinstance(token, str) or not hmac.compare_digest(
                token.encode("utf-8"), expected.encode("utf-8")):
            raise RpcError(ERR_UNAUTHORIZED, "invalid token")
        if params.get("protocol") != PROTOCOL_VERSION:
            raise RpcError(ERR_INVALID_REQUEST, "protocol version mismatch",
                           {"script": PROTOCOL_VERSION, "client": params.get("protocol")})
        return {
            "script_version": SCRIPT_VERSION,
            "protocol": PROTOCOL_VERSION,
            "code_hash": self._code_hash,
        }

    def _status(self):
        uptime = time.time() - self._started
        return {
            "ticks": self._ticks,
            "uptime": round(uptime, 3),
            "tick_rate": round(self._ticks / uptime, 2) if uptime > 0 else None,
            "clients": len(self._clients),
            "pending_requests": sum(c.inbuf.count(b"\n") for c in self._clients),
        }

    # ---------------------------------------------------------------- Live API

    def _live_error(self, exc, params):
        data = {"type": type(exc).__name__}
        if params.get("debug"):
            data["traceback"] = traceback.format_exc()
        return RpcError(ERR_LIVE, str(exc) or type(exc).__name__, data).to_dict()

    def _execute(self, method, params, in_batch):
        if method == "info":
            return self._m_info()
        if method == "get":
            return self._m_get(params)
        if method == "set":
            return self._m_set(params)
        if method == "call":
            return self._m_call(params)
        if method == "describe":
            return self._m_describe(params)
        if method == "batch" and not in_batch:
            return self._m_batch(params)
        raise RpcError(ERR_METHOD_NOT_FOUND, "unknown method: %s" % method)

    def _m_info(self):
        info = {
            "script_version": SCRIPT_VERSION,
            "protocol": PROTOCOL_VERSION,
            "code_hash": self._code_hash,
            "python_version": sys.version.split()[0],
            "platform": sys.platform,
            "linger": select is not None,
        }
        try:
            app = Live.Application.get_application()
            info["live_version"] = "%d.%d.%d" % (app.get_major_version(),
                                                 app.get_minor_version(),
                                                 app.get_bugfix_version())
        except Exception as exc:
            info["live_version"] = None
            info["live_version_error"] = str(exc)
        return info

    def _m_get(self, params):
        obj = self._resolve(params.get("target"))
        fields = _name_list(params.get("fields"), "fields")
        props = _name_list(params.get("props"), "props")
        if props is not None:
            return dict((name, self._encode(self._read_attr(obj, name), fields))
                        for name in props)
        return self._encode(obj, fields, params.get("offset"), params.get("limit"))

    def _m_set(self, params):
        obj = self._resolve(params.get("target"))
        self._require_live_object(obj, "set")
        prop = _check_name(params.get("prop"))
        if "value" not in params:
            raise RpcError(ERR_INVALID_PARAMS, "value is required")
        value = self._decode(params["value"])
        try:
            setattr(obj, prop, value)
        except TypeError:  # includes Boost.Python.ArgumentError: Live wanted another type
            number = _number_from_text(value)
            if number is None:
                raise
            setattr(obj, prop, number)
        return {"value": self._encode(getattr(obj, prop))}

    def _m_call(self, params):
        obj = self._resolve(params.get("target"))
        self._require_live_object(obj, "call")
        name = _check_name(params.get("method"))
        fn = self._read_attr(obj, name)
        if not callable(fn):
            raise RpcError(ERR_INVALID_PARAMS, "%s is not a method" % name)
        args = params.get("args") or []
        kwargs = params.get("kwargs") or {}
        if not isinstance(args, list) or not isinstance(kwargs, dict):
            raise RpcError(ERR_INVALID_PARAMS, "args must be a list and kwargs an object")
        result = fn(*self._decode(args), **self._decode(kwargs))
        return self._encode(result, _name_list(params.get("fields"), "fields"))

    def _m_describe(self, params):
        obj = self._resolve(params.get("target"))
        cls = type(obj)
        include_values = bool(params.get("values"))
        properties, methods, types, observable = [], [], [], []
        values = {}
        for name in dir(obj):
            if name.startswith("_"):
                continue
            if name.startswith("add_") and name.endswith("_listener"):
                observable.append(name[len("add_"):-len("_listener")])
                continue
            if name.endswith("_has_listener") or (
                    name.startswith("remove_") and name.endswith("_listener")):
                continue
            try:
                value = getattr(obj, name)
            except Exception as exc:
                properties.append(name)
                if include_values:
                    values[name] = {"$error": str(exc)}
                continue
            if isinstance(value, type):  # nested classes and enums, e.g. Track.View
                types.append(name)
            elif callable(value) and not isinstance(getattr(cls, name, None), property):
                methods.append(name)
            else:
                properties.append(name)
                if include_values:
                    values[name] = self._encode(value)
        result = {
            "type": cls.__name__,
            "module": getattr(cls, "__module__", None),
            "properties": properties,
            "methods": methods,
            "types": types,
            "observable": observable,
        }
        if include_values:
            result["values"] = values
        return result

    def _m_batch(self, params):
        ops = params.get("ops")
        if not isinstance(ops, list):
            raise RpcError(ERR_INVALID_PARAMS, "ops must be a list")
        stop_on_error = params.get("stop_on_error", True)
        song = self.song()
        undo = bool(params.get("undo_step")) and hasattr(song, "begin_undo_step")
        results = []
        self._batch_results = results
        if undo:
            song.begin_undo_step()
        try:
            for op in ops:
                try:
                    if not isinstance(op, dict) or not isinstance(op.get("method"), str):
                        raise RpcError(ERR_INVALID_REQUEST, "each op needs a method")
                    op_params = op.get("params") or {}
                    if not isinstance(op_params, dict):
                        raise RpcError(ERR_INVALID_PARAMS, "op params must be an object")
                    value = self._execute(op["method"], op_params, True)
                    results.append({"ok": True, "result": value})
                except RpcError as exc:
                    results.append({"ok": False, "error": exc.to_dict()})
                except Exception as exc:
                    results.append({"ok": False, "error": self._live_error(exc, params)})
                if stop_on_error and not results[-1]["ok"]:
                    break
        finally:
            self._batch_results = None
            if undo:
                song.end_undo_step()
        return {"results": results}

    # --------------------------------------------------------- object plumbing

    def _resolve(self, target):
        if isinstance(target, str):
            return self._walk(None, target)
        if isinstance(target, dict):
            if "result" in target:
                base = self._batch_result_object(target["result"])
            elif "id" in target:
                base = self._lookup(target["id"])
            else:
                base = None
            path = target.get("path") or ""
            if not isinstance(path, str):
                raise RpcError(ERR_INVALID_PARAMS, "target path must be a string")
            if base is None and not path:
                raise RpcError(ERR_INVALID_PARAMS, "target needs an id or a path")
            return self._walk(base, path)
        raise RpcError(ERR_INVALID_PARAMS, "target must be a path string or {id, path}")

    def _batch_result_object(self, index):
        """The Live object returned by an earlier op of the running batch."""
        results = self._batch_results
        if results is None:
            raise RpcError(ERR_INVALID_PARAMS, "{\"result\": n} targets only work inside a batch")
        if not isinstance(index, int) or isinstance(index, bool) or not 0 <= index < len(results):
            raise RpcError(ERR_INVALID_PARAMS, "no earlier batch op with index %r" % (index,))
        entry = results[index]
        value = entry.get("result") if entry.get("ok") else None
        if isinstance(value, dict) and "value" in value and "$obj" not in value:
            value = value["value"]  # a `set` result
        if not (isinstance(value, dict) and "$obj" in value):
            raise RpcError(ERR_INVALID_PARAMS, "batch op %d did not return a Live object" % index)
        return self._lookup(value["$obj"])

    def _walk(self, base, path):
        tokens = path.split()
        obj = base
        if obj is None:
            root = tokens.pop(0) if tokens else ""
            if root in ("live_set", "song"):
                obj = self.song()
            elif root in ("live_app", "app"):
                obj = Live.Application.get_application()
            else:
                raise RpcError(ERR_INVALID_PARAMS,
                               "path must start with live_set or live_app, got %r" % root)
        for token in tokens:
            if token.isdigit():
                obj = self._index(obj, int(token), path)
            else:
                obj = self._read_attr(obj, token)
        return obj

    def _index(self, seq, index, path):
        try:
            return seq[index]
        except IndexError:
            raise RpcError(ERR_INVALID_PARAMS, "index %d out of range in %r" % (index, path))
        except TypeError:
            if not _is_sequence(seq):
                raise RpcError(ERR_INVALID_PARAMS, "cannot index into %s in %r"
                               % (type(seq).__name__, path))
            items = list(seq)
            if index >= len(items):
                raise RpcError(ERR_INVALID_PARAMS, "index %d out of range in %r" % (index, path))
            return items[index]

    def _read_attr(self, obj, name):
        _check_name(name)
        try:
            return getattr(obj, name)
        except AttributeError:
            raise RpcError(ERR_INVALID_PARAMS, "%s has no attribute %r"
                           % (type(obj).__name__, name))

    def _require_live_object(self, obj, action):
        if obj is None or isinstance(obj, _NOT_LIVE_OBJECTS) or callable(obj):
            raise RpcError(ERR_INVALID_PARAMS, "%s targets must be Live objects, got %s"
                           % (action, type(obj).__name__))

    def _remember(self, obj):
        ptr = getattr(obj, "_live_ptr", None)
        oid = int(ptr) if isinstance(ptr, int) and not isinstance(ptr, bool) else id(obj)
        registry = self._registry
        if oid in registry:
            registry.move_to_end(oid)
        registry[oid] = obj
        while len(registry) > MAX_REGISTRY:
            registry.popitem(last=False)
        return oid

    def _lookup(self, oid):
        try:
            return self._registry[int(oid)]
        except (KeyError, TypeError, ValueError):
            raise RpcError(ERR_STALE_REF, "unknown or expired object id %r; resolve it "
                           "again by path" % (oid,))

    def _encode(self, value, fields=None, offset=None, limit=None, depth=0):
        if value is None or isinstance(value, bool):
            return value
        if isinstance(value, int):  # includes Live's enums, which subclass int
            return int(value)
        if isinstance(value, float):
            return value if math.isfinite(value) else None
        if isinstance(value, str):
            return value
        if isinstance(value, (bytes, bytearray)):
            return bytes(value).decode("utf-8", "replace")
        if depth > 8:
            return {"$repr": repr(value)[:200]}
        if isinstance(value, dict):
            return dict((str(k), self._encode(v, None, depth=depth + 1))
                        for k, v in value.items())
        if _is_sequence(value):
            items = list(value)
            if offset is None and limit is None:
                return [self._encode(v, fields, depth=depth + 1) for v in items]
            start = max(int(offset or 0), 0)
            end = len(items) if limit is None else start + max(int(limit), 0)
            return {"total": len(items), "offset": start,
                    "items": [self._encode(v, fields, depth=depth + 1)
                              for v in items[start:end]]}
        if callable(value):
            return {"$callable": getattr(value, "__name__", repr(value)[:100])}
        out = {"$obj": self._remember(value), "type": type(value).__name__}
        if fields:
            values = {}
            for name in fields:
                try:
                    values[name] = self._encode(getattr(value, name), depth=depth + 1)
                except Exception as exc:
                    values[name] = {"$error": str(exc) or type(exc).__name__}
            out["fields"] = values
        return out

    def _decode(self, value):
        if isinstance(value, list):
            return [self._decode(v) for v in value]
        if isinstance(value, dict):
            if len(value) == 1 and "$ref" in value:
                return self._resolve(value["$ref"])
            if len(value) == 1 and "$tuple" in value:
                items = value["$tuple"]
                if not isinstance(items, list):
                    raise RpcError(ERR_INVALID_PARAMS, "$tuple must wrap a list")
                return tuple(self._decode(v) for v in items)
            if "$new" in value:
                return self._construct(value)
            return dict((k, self._decode(v)) for k, v in value.items())
        return value

    def _construct(self, spec):
        """{"$new": "Clip.MidiNoteSpecification", "args": [...], "kwargs": {...}}

        Only names inside the `Live` module can be constructed.
        """
        name = spec.get("$new")
        if not isinstance(name, str):
            raise RpcError(ERR_INVALID_PARAMS, "$new must name a class in the Live module")
        parts = name.split(".")
        if parts and parts[0] == "Live":
            parts = parts[1:]
        cls = Live
        for part in parts:
            cls = self._read_attr(cls, part)
        if not callable(cls):
            raise RpcError(ERR_INVALID_PARAMS, "%s is not constructible" % name)
        args = spec.get("args") or []
        kwargs = spec.get("kwargs") or {}
        if not isinstance(args, list) or not isinstance(kwargs, dict):
            raise RpcError(ERR_INVALID_PARAMS, "$new args must be a list and kwargs an object")
        return cls(*self._decode(args), **self._decode(kwargs))
