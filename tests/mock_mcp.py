#!/usr/bin/env python3
"""Minimal mock MCP server (newline-delimited JSON-RPC 2.0 over stdio).

Used by soma's test suite to exercise the MCP client end-to-end without any
external dependency. Implements: initialize, tools/list, tools/call for one
tool ("add") plus a deliberately failing tool ("boom").
"""
import json
import sys


def reply(msg_id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": msg_id, "result": result}) + "\n")
    sys.stdout.flush()


def reply_error(msg_id, code, message):
    sys.stdout.write(
        json.dumps({"jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": message}}) + "\n"
    )
    sys.stdout.flush()


TOOLS = [
    {
        "name": "add",
        "description": "add two numbers together and return their sum",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "number"}, "b": {"type": "number"}},
            "required": ["a", "b"],
        },
    },
    {
        "name": "boom",
        "description": "always fails, for error-path testing",
        "inputSchema": {"type": "object", "properties": {}},
    },
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except json.JSONDecodeError:
        continue
    method = msg.get("method", "")
    msg_id = msg.get("id")
    if method == "initialize":
        reply(
            msg_id,
            {
                "protocolVersion": msg.get("params", {}).get("protocolVersion", "2024-11-05"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-mcp", "version": "1.0.0"},
            },
        )
    elif method == "notifications/initialized":
        pass  # notification, no response
    elif method == "tools/list":
        reply(msg_id, {"tools": TOOLS})
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name")
        args = params.get("arguments", {})
        if name == "add":
            total = args.get("a", 0) + args.get("b", 0)
            text = str(int(total)) if float(total).is_integer() else str(total)
            reply(msg_id, {"content": [{"type": "text", "text": text}], "isError": False})
        elif name == "boom":
            reply(msg_id, {"content": [{"type": "text", "text": "kaboom"}], "isError": True})
        else:
            reply_error(msg_id, -32602, f"unknown tool {name}")
    elif msg_id is not None:
        reply_error(msg_id, -32601, f"method not found: {method}")
