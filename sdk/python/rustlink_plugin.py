"""rust-link plugin SDK for Python 3.9+ (standard library only).

Copy this file next to your plugin (``rust-link plugin new <name>`` does it for you)::

    from rustlink_plugin import Plugin, track

    plugin = Plugin(version="1.0.0", source_name="radio", cache=False)

    @plugin.search("radio")
    def search(query):
        return [track(identifier="lofi", title="Lofi radio", uri="https://radio.example/lofi.mp3", is_stream=True)]

    plugin.run()

The node starts the plugin, talks JSON-RPC over stdin/stdout and logs whatever the
plugin writes to stderr. ``print()`` is redirected to stderr, so it is safe to use.
"""

import inspect
import itertools
import json
import os
import queue
import sys
import threading
import traceback
from concurrent.futures import ThreadPoolExecutor
from urllib.parse import urlencode

__all__ = ["Plugin", "PluginError", "Request", "Response", "track", "playlist", "error"]

PERIODIC = ("playerUpdate", "stats")


class PluginError(Exception):
    """Raise from a handler to answer with an error message (shown to the bot)."""

    def __init__(self, message, code=-32000):
        super().__init__(message)
        self.code = code


def track(identifier=None, title=None, author=None, length=None, uri=None, is_stream=None,
          is_seekable=None, artwork_url=None, isrc=None, source_name=None, plugin_info=None,
          user_data=None):
    """A track for a load result. Missing fields are filled in by the node; ``sourceName``
    defaults to the plugin's ``source_name``."""
    info = {
        "identifier": identifier,
        "title": title,
        "author": author,
        "length": length,
        "uri": uri,
        "isStream": is_stream,
        "isSeekable": is_seekable,
        "artworkUrl": artwork_url,
        "isrc": isrc,
        "sourceName": source_name,
    }
    result = {"info": {k: v for k, v in info.items() if v is not None}}
    if plugin_info:
        result["pluginInfo"] = plugin_info
    if user_data:
        result["userData"] = user_data
    return result


def playlist(name, tracks, selected=-1, plugin_info=None):
    """A playlist load result."""
    return {
        "loadType": "playlist",
        "data": {
            "info": {"name": name, "selectedTrack": selected},
            "pluginInfo": plugin_info or {},
            "tracks": list(tracks),
        },
    }


def error(message, severity="common"):
    """A load result telling the bot that loading failed."""
    return {"loadType": "error", "data": {"message": message, "severity": severity}}


class Response:
    """A route answer with a status code and headers: ``Response(body, status=201)``."""

    def __init__(self, body=None, status=None, headers=None):
        self.body = body
        self.status = status
        self.headers = headers or {}

    def envelope(self):
        result = {"body": self.body, "headers": self.headers}
        if self.status is not None:
            result["status"] = self.status
        return result


class Request:
    """A REST request routed to the plugin."""

    def __init__(self, params):
        self.route = params.get("route")
        self.method = params.get("method", "GET")
        self.path = params.get("path", "/")
        self.params = params.get("params") or {}
        self.query = params.get("query") or {}
        self.headers = params.get("headers") or {}
        self.body = params.get("body")

    def __repr__(self):
        return "Request({} {})".format(self.method, self.path)


def _call(fn, *args):
    """Call ``fn`` with as many leading arguments as it accepts."""
    try:
        parameters = inspect.signature(fn).parameters.values()
    except (TypeError, ValueError):
        return fn(*args)
    if any(p.kind == p.VAR_POSITIONAL for p in parameters):
        return fn(*args)
    positional = [p for p in parameters if p.kind in (p.POSITIONAL_ONLY, p.POSITIONAL_OR_KEYWORD)]
    return fn(*args[: len(positional)])


def _route_key(method, path):
    return "{} {}".format(method.upper(), path)


class Plugin:
    def __init__(self, name=None, version="0.1.0", source_name=None, cache=True, workers=16):
        self.name = name or os.environ.get("RUSTLINK_PLUGIN", "plugin")
        self.version = version
        self.source_name = source_name
        self.cache = cache
        #: Settings from ``plugins.<name>`` in application.yml (filled at startup).
        self.config = {}
        #: Node details: name, version, lavalinkApi, address, port.
        self.node = {}
        self._prefixes = {}
        self._urls = []
        self._resolver = None
        self._events = {}
        self._routes = {}
        self._on_start = []
        self._on_stop = []
        self._out = sys.stdout
        sys.stdout = sys.stderr
        self._write_lock = threading.Lock()
        self._ids = itertools.count(1)
        self._pending = {}
        self._pending_lock = threading.Lock()
        self._pool = ThreadPoolExecutor(max_workers=workers)
        self._event_queue = queue.Queue()

    # ----------------------------------------------------------------- registration

    def search(self, *prefixes):
        """Handle ``prefix:query`` identifiers. The handler gets ``(query, identifier)``."""

        def register(fn):
            for prefix in prefixes:
                self._prefixes[prefix.strip().rstrip(":").lower()] = fn
            return fn

        return register

    def url(self, pattern):
        """Handle identifiers matching a regular expression (Rust ``regex`` syntax, which
        matches Python's for common patterns). The handler gets ``(url, match)``."""
        import re

        compiled = re.compile(pattern)

        def register(fn):
            self._urls.append((pattern, compiled, fn))
            return fn

        return register

    def resolver(self, fn):
        """Choose where the plugin's tracks play from. The handler gets the track info and
        returns ``{"url": ..., "headers": {...}}``, ``{"file": path}``,
        ``{"identifier": "ytsearch:..."}``, ``{"track": {...}}`` or ``None`` (use the URI)."""
        self._resolver = fn
        return fn

    def on(self, *names):
        """Receive websocket messages: event types (``TrackStartEvent``), ops (``ready``,
        ``playerUpdate``, ``stats``) or ``*`` for all but the periodic ones. The handler
        gets ``(message, session_id)``."""

        def register(fn):
            for name in names:
                self._events.setdefault(name, []).append(fn)
            return fn

        return register

    def route(self, method, path):
        """Serve a REST route, e.g. ``@plugin.route("GET", "/v4/lyrics/{guildId}")``.
        ``{*rest}`` as the last segment matches the remainder of the path. The handler gets
        a :class:`Request` and returns the body (any JSON value; a string is sent as text),
        ``(status, body)`` or a :class:`Response`."""

        def register(fn):
            self._routes[_route_key(method, path)] = (method.upper(), path, fn)
            return fn

        return register

    def on_start(self, fn):
        """Run once the node has sent the settings (``fn(config, node)``)."""
        self._on_start.append(fn)
        return fn

    def on_stop(self, fn):
        """Run before the node stops the plugin."""
        self._on_stop.append(fn)
        return fn

    # ----------------------------------------------------------------- node access

    def log(self, message, level="info"):
        self._send({"jsonrpc": "2.0", "method": "log", "params": {"level": level, "message": str(message)}})

    def rest(self, method, path, body=None, query=None, timeout=30):
        """Call the node's REST API (no password needed). Returns ``(status, body)``.
        Available once the node has finished starting (not inside ``on_start``)."""
        if query:
            path = "{}{}{}".format(path, "&" if "?" in path else "?", urlencode(query))
        result = self._request("rest", {"method": method, "path": path, "body": body}, timeout)
        return result.get("status"), result.get("body")

    def load(self, identifier):
        """``/v4/loadtracks`` through the node: any source, including other plugins."""
        status, body = self.rest("GET", "/v4/loadtracks", query={"identifier": identifier})
        if status != 200:
            raise PluginError("loadtracks failed with status {}: {}".format(status, body))
        return body

    # ----------------------------------------------------------------- protocol

    def manifest(self):
        return {
            "name": self.name,
            "version": self.version,
            "sourceName": self.source_name,
            "searchPrefixes": sorted(self._prefixes),
            "urlPatterns": [pattern for pattern, _, _ in self._urls],
            "events": sorted(self._events),
            "routes": [{"method": m, "path": p} for m, p, _ in self._routes.values()],
            "resolve": self._resolver is not None,
            "cache": self.cache,
        }

    def _send(self, message):
        line = json.dumps(message, ensure_ascii=False, separators=(",", ":"))
        with self._write_lock:
            self._out.write(line + "\n")
            self._out.flush()

    def _request(self, method, params, timeout):
        request_id = next(self._ids)
        slot = {"event": threading.Event()}
        with self._pending_lock:
            self._pending[request_id] = slot
        self._send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        if not slot["event"].wait(timeout):
            with self._pending_lock:
                self._pending.pop(request_id, None)
            raise PluginError("node did not answer {} within {} s".format(method, timeout))
        if "error" in slot:
            raise PluginError(slot["error"].get("message", "node error"))
        return slot.get("result") or {}

    def _handle(self, method, params):
        if method == "initialize":
            self.config = params.get("config") or {}
            self.node = params.get("node") or {}
            for fn in self._on_start:
                _call(fn, self.config, self.node)
            return self.manifest()
        if method == "loadItem":
            identifier = params.get("identifier", "")
            prefix = params.get("prefix")
            if prefix is not None and prefix in self._prefixes:
                return _call(self._prefixes[prefix], params.get("query", ""), identifier)
            for _, compiled, fn in self._urls:
                match = compiled.search(identifier)
                if match:
                    return _call(fn, identifier, match)
            return None
        if method == "resolve":
            if self._resolver is None:
                return None
            return _call(self._resolver, params.get("track") or {})
        if method == "route":
            entry = self._routes.get(params.get("route", ""))
            if entry is None:
                raise PluginError("no handler for {}".format(params.get("route")), -32601)
            answer = _call(entry[2], Request(params))
            if isinstance(answer, Response):
                return answer.envelope()
            if isinstance(answer, tuple) and len(answer) == 2:
                return {"status": answer[0], "body": answer[1]}
            return {"body": answer}
        raise PluginError("unknown method {}".format(method), -32601)

    def _run_request(self, request_id, method, params):
        try:
            result = self._handle(method, params)
            reply = {"jsonrpc": "2.0", "id": request_id, "result": result}
        except PluginError as e:
            reply = {"jsonrpc": "2.0", "id": request_id, "error": {"code": e.code, "message": str(e)}}
        except Exception as e:  # noqa: BLE001 - reported to the node
            traceback.print_exc()
            reply = {
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32000, "message": "{}: {}".format(type(e).__name__, e)},
            }
        self._send(reply)

    def _dispatch_events(self):
        while True:
            params = self._event_queue.get()
            if params is None:
                return
            message = params.get("message") or {}
            names = [message.get("type"), message.get("op")]
            handlers = [fn for name in names if name for fn in self._events.get(name, [])]
            if message.get("op") not in PERIODIC:
                handlers += self._events.get("*", [])
            for fn in handlers:
                try:
                    _call(fn, message, params.get("sessionId"))
                except Exception:  # noqa: BLE001 - keep receiving events
                    traceback.print_exc()

    def _stop(self):
        for fn in self._on_stop:
            try:
                fn()
            except Exception:  # noqa: BLE001
                traceback.print_exc()
        self._event_queue.put(None)

    def run(self):
        """Serve the node until it closes stdin or asks the plugin to stop."""
        threading.Thread(target=self._dispatch_events, name="events", daemon=True).start()
        stdin = sys.stdin.buffer
        for raw in stdin:
            line = raw.decode("utf-8", "replace").strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except ValueError:
                print("rust-link sent invalid JSON: " + line[:200])
                continue
            method = message.get("method")
            request_id = message.get("id")
            if method is None:
                with self._pending_lock:
                    slot = self._pending.pop(request_id, None)
                if slot is not None:
                    if message.get("error"):
                        slot["error"] = message["error"]
                    else:
                        slot["result"] = message.get("result")
                    slot["event"].set()
            elif method == "shutdown":
                break
            elif method == "event":
                self._event_queue.put(message.get("params") or {})
            elif request_id is not None:
                self._pool.submit(self._run_request, request_id, method, message.get("params") or {})
        self._stop()
        self._pool.shutdown(wait=False, cancel_futures=True)
        sys.stderr.flush()
        # Worker threads may still wait on the node; nothing else is left to do.
        os._exit(0)
