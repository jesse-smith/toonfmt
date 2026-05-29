#!/usr/bin/env python3
"""Minimal hand-rolled MCP stdio server — zero deps, stdlib only.

Two jobs:
  1. Routing probe (now): emit a `tools/call` result whose `content` text block
     and `structuredContent` carry *different, labelled* sentinels, so a real
     client session reveals which channel the model actually ingests. We hand-roll
     the raw CallToolResult precisely because frameworks (FastMCP etc.) auto-mirror
     structured output into a content block, which would make the two channels
     identical and destroy the signal.
  2. Phase 2 fixture (later): the `content` block is a JSON-object *string*, so it
     exercises toonfmt's JSON->TOON transform (dbmcp is TOON-native -> a no-op).

Protocol: newline-delimited JSON-RPC over stdin/stdout. One message per line.
Logs go to stderr only; stdout is the protocol channel exclusively.
"""

import json
import sys

PROTOCOL_VERSION = "2025-06-18"

# The tool's content block: a JSON object encoded as a string. The model, if it
# reads `content`, should see `routing_probe` / `seen_via=content`.
CONTENT_PAYLOAD = {
    "routing_probe": "CONTENT_CHANNEL",
    "seen_via": "content",
    "note": "If you can read this, the client forwards content[].text to the model.",
    "rows": [
        {"id": 1, "name": "alice", "role": "admin"},
        {"id": 2, "name": "bob", "role": "user"},
    ],
}

# structuredContent: a DIFFERENT object with its own sentinel. If the model can
# also see `STRUCTURED_CHANNEL`, the client forwards structuredContent too.
STRUCTURED_PAYLOAD = {
    "routing_probe": "STRUCTURED_CHANNEL",
    "seen_via": "structuredContent",
    "note": "If you can read this, the client ALSO forwards structuredContent.",
    "rows": [
        {"id": 1, "name": "alice", "role": "admin"},
        {"id": 2, "name": "bob", "role": "user"},
    ],
}

# A NON-OBJECT structuredContent: a TOON *string*, violating the wire type
# (spec types structuredContent as a JSON object). Question this probes: does
# Claude Code tolerate / forward a string-valued structuredContent at all? If
# the model can read STRUCTURED_STRING_CHANNEL, then TOON-in-structuredContent
# (option 2b) is at least viable on this client; if it sees the content
# sentinel instead, a non-object structuredContent is ignored (2b is dead and
# strip/add-content wins); if neither, the client choked on the bad type.
STRUCTURED_STRING_PAYLOAD = (
    "routing_probe: STRUCTURED_STRING_CHANNEL\n"
    "seen_via: structuredContent_as_string\n"
    "note: If you can read this, the client forwards a STRING-valued structuredContent.\n"
    "rows[2]{id,name,role}:\n"
    "  1,alice,admin\n"
    "  2,bob,user\n"
)

# Spec-compliant SHOULD case: the content text block is the serialized JSON of the
# *same* object as structuredContent. This is the equality-gated strip target —
# toonfmt should TOON the content block AND drop the now-redundant structuredContent.
EQUAL_PAIR_PAYLOAD = {
    "report": "quarterly",
    "rows": [
        {"id": 1, "name": "alice", "amount": 100},
        {"id": 2, "name": "bob", "amount": 250},
    ],
}

def _empty_schema() -> dict:
    return {"type": "object", "properties": {}, "additionalProperties": False}


# Three tools to fully map the client's content-vs-structuredContent routing:
#   probe_both            -> both channels present (precedence test)
#   probe_content_only    -> only content (does content reach the model when alone?)
#   probe_structured_only -> only structuredContent (confirm structured alone)
TOOLS = [
    {
        "name": "probe_both",
        "description": "Returns labelled sentinels in BOTH content text and structuredContent (precedence test).",
        "inputSchema": _empty_schema(),
    },
    {
        "name": "probe_content_only",
        "description": "Returns a sentinel ONLY in the content text block; no structuredContent. Make-or-break: is content forwarded when alone?",
        "inputSchema": _empty_schema(),
    },
    {
        "name": "probe_structured_only",
        "description": "Returns a sentinel ONLY in structuredContent; content is an empty array.",
        "inputSchema": _empty_schema(),
    },
    {
        "name": "probe_structured_string",
        "description": "Returns a content sentinel AND a STRING-valued (TOON) structuredContent, violating the object wire type. Probes whether the client tolerates/forwards a non-object structuredContent.",
        "inputSchema": _empty_schema(),
    },
    {
        "name": "probe_equal_pair",
        "description": "Spec-compliant SHOULD case: content text block is the serialized JSON of the SAME object as structuredContent. Exercises toonfmt's equality-gated strip end-to-end.",
        "inputSchema": _empty_schema(),
    },
]


def log(msg: str) -> None:
    print(f"[json_mcp_stub] {msg}", file=sys.stderr, flush=True)


def send(obj: dict) -> None:
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def _content_block() -> dict:
    # content[].text is a JSON *string* -> transform target in Phase 2.
    return {"type": "text", "text": json.dumps(CONTENT_PAYLOAD)}


def make_result(req_id, tool_name) -> dict:
    if tool_name == "probe_content_only":
        result = {"content": [_content_block()], "isError": False}
    elif tool_name == "probe_equal_pair":
        # content text == serialized structuredContent (same object): strip case.
        result = {
            "content": [{"type": "text", "text": json.dumps(EQUAL_PAIR_PAYLOAD)}],
            "structuredContent": EQUAL_PAIR_PAYLOAD,
            "isError": False,
        }
    elif tool_name == "probe_structured_only":
        result = {"content": [], "structuredContent": STRUCTURED_PAYLOAD, "isError": False}
    elif tool_name == "probe_structured_string":
        # Deliberately type-violating: structuredContent is a STRING, not an object.
        result = {
            "content": [_content_block()],
            "structuredContent": STRUCTURED_STRING_PAYLOAD,
            "isError": False,
        }
    else:  # probe_both (and any default)
        result = {
            "content": [_content_block()],
            "structuredContent": STRUCTURED_PAYLOAD,
            "isError": False,
        }
    return {"jsonrpc": "2.0", "id": req_id, "result": result}


def handle(msg: dict):
    method = msg.get("method")
    req_id = msg.get("id")

    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "result": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "json-mcp-stub", "version": "0.1.0"},
            },
        })
    elif method == "notifications/initialized":
        pass  # notification: no response
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": req_id, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        tool_name = (msg.get("params") or {}).get("name")
        send(make_result(req_id, tool_name))
    elif method == "ping":
        send({"jsonrpc": "2.0", "id": req_id, "result": {}})
    elif req_id is not None:
        # Unknown request -> JSON-RPC method-not-found (notifications get silence).
        send({
            "jsonrpc": "2.0",
            "id": req_id,
            "error": {"code": -32601, "message": f"method not found: {method}"},
        })


def main() -> None:
    log("started; reading JSON-RPC on stdin")
    for raw in sys.stdin:
        line = raw.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            log(f"skipping non-JSON line: {line!r}")
            continue
        handle(msg)
    log("stdin closed; exiting")


if __name__ == "__main__":
    main()
