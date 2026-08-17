#!/usr/bin/env python3
"""Minimal streamable-HTTP MCP server for ACP E2E validation.

Implements just enough of the 2025-11-25 streamable HTTP transport:
  POST /mcp  -> initialize, notifications/initialized, tools/list,
                tools/call, ping
The `echo` tool appends to TOOL_LOG so a driver can assert a real call.

Usage: python3 mcp_server.py --port <port> --log <path>
"""

import argparse
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOOL_LOG = "/tmp/acp-e2e/tool_calls.log"


class McpHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _read_body(self):
        length = int(self.headers.get("Content-Length", 0))
        return self.rfile.read(length) if length else b""

    def _reply_json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def _reply_202(self):
        self.send_response(202)
        self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()

    def do_POST(self):
        raw = self._read_body().decode()
        try:
            msg = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            self._reply_json({"jsonrpc": "2.0", "error": {"code": -32700, "message": "parse error"}})
            return

        method = msg.get("method", "")
        rid = msg.get("id")
        if rid is None:
            # Notification (e.g. notifications/initialized) — acknowledge silently.
            self._reply_202()
            return

        if method == "initialize":
            result = {
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "e2e-mcp", "version": "1.0"},
            }
        elif method == "tools/list":
            result = {
                "tools": [{
                    "name": "echo",
                    "description": "Echoes back the message argument",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"message": {"type": "string"}},
                    },
                }]
            }
        elif method == "tools/call":
            params = msg.get("params", {})
            name = params.get("name", "")
            args = params.get("arguments", {}) or {}
            if name == "echo":
                text = str(args.get("message", ""))
                with open(TOOL_LOG, "a", encoding="utf-8") as f:
                    f.write(json.dumps({"tool": name, "message": text}) + "\n")
                result = {
                    "content": [{"type": "text", "text": f"ECHO:{text}"}],
                    "isError": False,
                }
            else:
                result = {
                    "content": [{"type": "text", "text": f"unknown tool: {name}"}],
                    "isError": True,
                }
        elif method == "ping":
            result = {}
        else:
            result = {"note": f"unhandled:{method}"}

        self._reply_json({"jsonrpc": "2.0", "id": rid, "result": result})

    def do_GET(self):
        # Streamable HTTP SSE GET: send headers and keep the stream open.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.end_headers()
        self.wfile.write(b": connected\n\n")
        self.wfile.flush()


def main():
    global TOOL_LOG
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--log", default=TOOL_LOG)
    args = parser.parse_args()
    TOOL_LOG = args.log
    with open(TOOL_LOG, "w", encoding="utf-8"):
        pass
    server = ThreadingHTTPServer(("127.0.0.1", args.port), McpHandler)
    print(f"MCP_SERVER_READY {args.port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
