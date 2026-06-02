#!/usr/bin/env python3
"""Minimal hand-rolled **Streamable HTTP** MCP server — zero deps, stdlib only.

Sibling to `json_mcp_stub.py` (the stdio stub). Same job — emit `tools/call`
results whose `content[].text` is a JSON-object *string*, so toonfmt's JSON->TOON
transform has something to bite — but delivered over the MCP Streamable HTTP
transport instead of stdio pipes. The point of the three tool probes is to vary
only the *framing* (plain JSON body vs. SSE vs. multi-`data:`-line SSE) while
holding the payload constant, so the e2e test proves the transform fires
identically regardless of how the bytes arrived.

Transport (MCP Streamable HTTP, 2025-03-26):
  - One endpoint (path is ignored — any path POSTs here).
  - POST `initialize` -> assigns an `Mcp-Session-Id`, returned as a response header.
  - Subsequent POSTs echo that header; we validate loosely (warn, don't reject).
  - A POST that carries a request (`id` present) gets a response whose framing
    depends on the tool: `application/json` for one probe, `text/event-stream`
    (SSE) for the others.
  - A POST that carries only a notification (no `id`) -> 202 Accepted, empty body.
  - GET (the optional server->client stream) -> 405: we don't open one. The e2e
    watches for toonfmt's GET-stream warning to confirm nothing is silently dropped.
  - DELETE -> 200: session teardown, no-op here.

Port discovery: bind 127.0.0.1:0 (ephemeral) and print `PORT <n>` on the FIRST
line of stdout, flushed, before serving. The e2e reads exactly that line, then
connects. Logs otherwise go to stderr; stdout carries only the port line.
"""

import json
import os
import sys
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

PROTOCOL_VERSION = "2025-03-26"

# --- OAuth mode (opt-in via env, so the bearer/no-auth tests are untouched) -------
# When TOONFMT_STUB_OAUTH=1, the stub also serves a minimal OAuth 2.1 + dynamic
# client registration surface (discovery, register, authorize->302, token) and the
# MCP endpoint requires the bearer the stub issued. The explicit-flow e2e (B5)
# exercises this; Slice A's tests leave it off and see the original behavior.
OAUTH_MODE = os.environ.get("TOONFMT_STUB_OAUTH") == "1"

# The single access token this stub will mint + accept, and the refresh token it
# hands out. Fixed strings keep the e2e assertions simple; rotation is exercised by
# the refresh endpoint returning a *second* access token.
ISSUED_ACCESS_TOKEN = "stub-access-token-v1"
REFRESHED_ACCESS_TOKEN = "stub-access-token-v2"
ISSUED_REFRESH_TOKEN = "stub-refresh-token"
# An authorization code the authorize endpoint redirects with, exchanged at /token.
ISSUED_CODE = "stub-auth-code"

# The shared content payload: a JSON object serialized to a *string*, placed in
# content[].text. Identical across all three probes — only the HTTP framing
# differs — so the transform's behavior is what's isolated, not the data.
CONTENT_PAYLOAD = {
    "routing_probe": "CONTENT_CHANNEL",
    "seen_via": "content",
    "note": "If this is TOON on the client side, the HTTP-leg transform fired.",
    "rows": [
        {"id": 1, "name": "alice", "role": "admin"},
        {"id": 2, "name": "bob", "role": "user"},
    ],
}


def _empty_schema() -> dict:
    return {"type": "object", "properties": {}, "additionalProperties": False}


# Three probes — identical payload, three framings:
#   probe_content_only -> application/json body (the plain path)
#   probe_sse          -> text/event-stream, JSON on a single `data:` line
#   probe_sse_split    -> text/event-stream, JSON split across multiple `data:`
#                         lines in one event (the buffer-until-complete canary)
TOOLS = [
    {
        "name": "probe_content_only",
        "description": "tools/call result as a plain application/json body; content[].text is a JSON string.",
        "inputSchema": _empty_schema(),
    },
    {
        "name": "probe_sse",
        "description": "Same payload returned as a single-line-data SSE event (text/event-stream).",
        "inputSchema": _empty_schema(),
    },
    {
        "name": "probe_sse_split",
        "description": "Same payload as an SSE event whose JSON is split across multiple data: lines (buffering canary).",
        "inputSchema": _empty_schema(),
    },
    {
        "name": "probe_auth_seen",
        "description": "Returns the Authorization header the server observed, as a JSON-string content block.",
        "inputSchema": _empty_schema(),
    },
]

# The most recent `Authorization` header the server saw on any POST. Surfaced via
# the `probe_auth_seen` tool's *content* (which rmcp preserves verbatim — A1),
# NOT via initialize's serverInfo (rmcp's typed `Implementation` struct drops
# unknown fields, so an echo there would not survive the round-trip). This lets an
# e2e assert on the wire that `--bearer-env` produced `Authorization: Bearer <tok>`
# and that no-bearer sends no header — behavior a Rust unit test of token
# *resolution* cannot reach.
_last_auth_header = None


def log(msg: str) -> None:
    print(f"[http_mcp_stub] {msg}", file=sys.stderr, flush=True)


def _content_block() -> dict:
    # content[].text is a JSON *string* -> the JSON->TOON transform target.
    return {"type": "text", "text": json.dumps(CONTENT_PAYLOAD)}


def _tools_call_result(req_id, tool_name) -> dict:
    # All three framing probes carry the identical content block; framing is
    # chosen by the caller (handler) based on tool_name, not here.
    if tool_name in ("probe_content_only", "probe_sse", "probe_sse_split"):
        result = {"content": [_content_block()], "isError": False}
    elif tool_name == "probe_auth_seen":
        # Echo the observed Authorization header as a JSON-object string, so it
        # both verifies auth AND rides the transform path (content survives rmcp).
        payload = {"authorization": _last_auth_header}
        result = {
            "content": [{"type": "text", "text": json.dumps(payload)}],
            "isError": False,
        }
    else:
        return {
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32602, "message": f"unknown tool: {tool_name}"},
        }
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def _response_for(msg: dict):
    """Map one client JSON-RPC message to (kind, payload).

    kind is 'json' (one envelope -> application/json), 'sse'/'sse_split' (one
    envelope -> text/event-stream), or 'accepted' (a notification -> 202).
    """
    method = msg.get("method")
    req_id = msg.get("id")

    if method == "initialize":
        env = {
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "http-mcp-stub", "version": "0.1.0"},
            },
        }
        return ("json", env)
    if method == "notifications/initialized" or req_id is None:
        return ("accepted", None)
    if method == "tools/list":
        return ("json", {"jsonrpc": "2.0", "id": req_id, "result": {"tools": TOOLS}})
    if method == "ping":
        return ("json", {"jsonrpc": "2.0", "id": req_id, "result": {}})
    if method == "tools/call":
        tool_name = (msg.get("params") or {}).get("name")
        env = _tools_call_result(req_id, tool_name)
        # Choose framing by tool: one plain JSON, two SSE variants.
        if tool_name == "probe_sse":
            return ("sse", env)
        if tool_name == "probe_sse_split":
            return ("sse_split", env)
        return ("json", env)
    # Unknown request -> method-not-found.
    return (
        "json",
        {
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32601, "message": f"method not found: {method}"},
        },
    )


def _sse_single(env: dict) -> bytes:
    """One SSE event, whole JSON on a single `data:` line."""
    return f"event: message\ndata: {json.dumps(env)}\n\n".encode("utf-8")


def _sse_split(env: dict) -> bytes:
    """One SSE event whose JSON is split across multiple `data:` lines.

    SSE joins consecutive `data:` lines with `\\n` before dispatch, so the
    receiver must reassemble the whole event before it sees valid JSON. We
    pretty-print the envelope so every newline already sits at a structurally
    *insignificant* position (between tokens, never inside a string literal),
    then emit one `data:` line per physical line. The `\\n` the SSE joiner
    re-inserts reproduces exactly the indented JSON -> it parses cleanly; but any
    single `data:` line alone (e.g. `  "id": 4,`) is NOT valid JSON. That's the
    buffer-until-complete canary: a consumer that transforms per-line sees only
    fragments and must not fire.

    (Splitting on raw commas would be wrong: commas inside the escaped inner JSON
    string in content[].text would put the joiner's `\\n` *inside* a string
    literal — an invalid control character. Indentation-based splits are safe by
    construction.)
    """
    lines = json.dumps(env, indent=2).split("\n")
    data_lines = "".join(f"data: {line}\n" for line in lines)
    return f"event: message\n{data_lines}\n".encode("utf-8")


class Handler(BaseHTTPRequestHandler):
    # Class-level session store (one stub, one logical session is enough).
    session_id = None

    def log_message(self, *args) -> None:  # silence stdlib's stderr access log
        pass

    def _read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length", 0))
        return self.rfile.read(length) if length else b""

    def _send_json(self, status: int, obj: dict) -> None:
        body = json.dumps(obj).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        path = urlparse(self.path).path

        # --- OAuth discovery (only in OAuth mode) ---
        if OAUTH_MODE and path == "/.well-known/oauth-authorization-server":
            base = f"http://{self.headers.get('Host', '127.0.0.1')}"
            self._send_json(
                200,
                {
                    "issuer": base,
                    "authorization_endpoint": f"{base}/authorize",
                    "token_endpoint": f"{base}/token",
                    "registration_endpoint": f"{base}/register",
                    "response_types_supported": ["code"],
                    "code_challenge_methods_supported": ["S256"],
                    "grant_types_supported": ["authorization_code", "refresh_token"],
                },
            )
            return

        # --- OAuth authorize: 302 straight back to the client's redirect_uri with
        # code+state. A real server would render a consent page; the stub
        # auto-approves so the headless test browser (a plain GET that follows
        # redirects) completes the loopback callback with no human. ---
        if OAUTH_MODE and path == "/authorize":
            params = parse_qs(urlparse(self.path).query)
            redirect_uri = params.get("redirect_uri", [None])[0]
            state = params.get("state", [""])[0]
            if not redirect_uri:
                self.send_response(400)
                self.end_headers()
                return
            sep = "&" if "?" in redirect_uri else "?"
            location = f"{redirect_uri}{sep}code={ISSUED_CODE}&state={state}"
            self.send_response(302)
            self.send_header("Location", location)
            self.end_headers()
            return

        # The optional server->client SSE stream. We don't open one; 405 tells
        # the client it's unsupported. toonfmt should warn (not silently drop)
        # if it ever expects this — the e2e checks for that warning's absence.
        self.send_response(405)
        self.send_header("Allow", "POST, DELETE")
        self.end_headers()

    def do_DELETE(self) -> None:
        self.send_response(200)
        self.end_headers()

    def do_POST(self) -> None:
        path = urlparse(self.path).path

        # --- OAuth dynamic client registration ---
        if OAUTH_MODE and path == "/register":
            raw = self._read_body()
            try:
                req = json.loads(raw)
            except json.JSONDecodeError:
                self.send_response(400)
                self.end_headers()
                return
            # Echo back a public-client registration. client_id is fixed so the
            # e2e can assert on it; redirect_uris echoed per the request.
            self._send_json(
                201,
                {
                    "client_id": "stub-client-id",
                    "client_name": req.get("client_name", "toonfmt"),
                    "redirect_uris": req.get("redirect_uris", []),
                    "grant_types": req.get("grant_types", []),
                    "token_endpoint_auth_method": "none",
                },
            )
            return

        # --- OAuth token endpoint: authorization_code exchange AND refresh_token ---
        if OAUTH_MODE and path == "/token":
            raw = self._read_body()
            form = parse_qs(raw.decode("utf-8"))
            grant = form.get("grant_type", [""])[0]
            if grant == "authorization_code":
                access = ISSUED_ACCESS_TOKEN
            elif grant == "refresh_token":
                # Rotation: a refresh yields a *different* access token, so a test
                # can prove the refreshed token is what gets persisted/used.
                access = REFRESHED_ACCESS_TOKEN
            else:
                self._send_json(400, {"error": "unsupported_grant_type"})
                return
            self._send_json(
                200,
                {
                    "access_token": access,
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "refresh_token": ISSUED_REFRESH_TOKEN,
                    "scope": "mcp",
                },
            )
            return

        # Record the Authorization header seen on every POST (surfaced via the
        # probe_auth_seen tool's content — see _last_auth_header).
        global _last_auth_header
        _last_auth_header = self.headers.get("Authorization")

        # In OAuth mode the MCP endpoint requires the bearer we issued. This is what
        # makes the e2e meaningful: a serve path that didn't load+attach the token
        # gets 401, not a silent success.
        if OAUTH_MODE:
            expected = {f"Bearer {ISSUED_ACCESS_TOKEN}", f"Bearer {REFRESHED_ACCESS_TOKEN}"}
            if _last_auth_header not in expected:
                self.send_response(401)
                self.send_header("WWW-Authenticate", 'Bearer realm="mcp"')
                self.end_headers()
                return

        raw = self._read_body()
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            log(f"non-JSON POST body: {raw!r}")
            self.send_response(400)
            self.end_headers()
            return

        # Loose session validation: assign on initialize, warn on mismatch.
        if msg.get("method") == "initialize":
            Handler.session_id = uuid.uuid4().hex
        else:
            got = self.headers.get("Mcp-Session-Id")
            if Handler.session_id and got != Handler.session_id:
                log(f"session id mismatch: got {got!r}, have {Handler.session_id!r}")

        kind, payload = _response_for(msg)

        if kind == "accepted":
            self.send_response(202)
            if Handler.session_id:
                self.send_header("Mcp-Session-Id", Handler.session_id)
            self.end_headers()
            return

        if kind == "json":
            body = (json.dumps(payload) + "\n").encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            if Handler.session_id:
                self.send_header("Mcp-Session-Id", Handler.session_id)
            self.end_headers()
            self.wfile.write(body)
            return

        # SSE framings.
        body = _sse_single(payload) if kind == "sse" else _sse_split(payload)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Content-Length", str(len(body)))
        if Handler.session_id:
            self.send_header("Mcp-Session-Id", Handler.session_id)
        self.end_headers()
        self.wfile.write(body)


def main() -> None:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    port = server.server_address[1]
    # Contract with the e2e: the port is the FIRST line of stdout, flushed.
    sys.stdout.write(f"PORT {port}\n")
    sys.stdout.flush()
    log(f"serving Streamable HTTP MCP on 127.0.0.1:{port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
