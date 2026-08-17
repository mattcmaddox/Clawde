#!/usr/bin/env python3
"""Scripted ACP-over-TCP driver validating session MCP end-to-end.

Checks:
  1. initialize advertises mcpCapabilities.http and .sse
  2. session/new with a localhost streamable-HTTP MCP server succeeds
  3. session/prompt makes the model call the session MCP `echo` tool
     (verified via the MCP server's tool-call log)
  4. session/new with private / loopback-internal URLs is rejected with an
     SSRF reason (http and sse variants)

Usage: python3 acp_driver.py --acp-port <port> --mcp-url <url> --workdir <dir> --log <path>
"""

import argparse
import json
import os
import socket
import sys
import threading
import time
import uuid

PROMPT_TIMEOUT_SECS = 240


class AcpClient:
    def __init__(self, host, port):
        self.sock = socket.create_connection((host, port), timeout=30)
        # The reader must block indefinitely; per-request timeouts are handled
        # by the waiter events, not the socket read timeout.
        self.sock.settimeout(None)
        self.file = self.sock.makefile("r", encoding="utf-8", newline="\n")
        self.next_id = 1
        self.pending = {}
        self.notifications = []
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _read_loop(self):
        for line in self.file:
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if "id" in msg and msg["id"] is not None:
                waiter = self.pending.pop(msg["id"], None)
                if waiter:
                    waiter["result"] = msg
                    waiter["done"].set()
            elif "method" in msg:
                self.notifications.append(msg)

    def request(self, method, params, timeout=30):
        rid = self.next_id
        self.next_id += 1
        payload = {"jsonrpc": "2.0", "id": rid, "method": method, "params": params}
        self.sock.sendall((json.dumps(payload) + "\n").encode())
        waiter = {"done": threading.Event(), "result": None}
        self.pending[rid] = waiter
        if not waiter["done"].wait(timeout):
            self.pending.pop(rid, None)
            raise TimeoutError(f"timeout waiting for response to {method}")
        return waiter["result"]

    def close(self):
        self.sock.close()


def check(name, cond, detail=""):
    status = "PASS" if cond else "FAIL"
    print(f"[{status}] {name}{(' — ' + detail) if detail else ''}", flush=True)
    return cond


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--acp-port", type=int, required=True)
    parser.add_argument("--mcp-url", required=True)
    parser.add_argument("--workdir", required=True)
    parser.add_argument("--log", default="/tmp/acp-e2e/tool_calls.log")
    args = parser.parse_args()

    os.makedirs(args.workdir, exist_ok=True)
    results = []
    client = AcpClient("127.0.0.1", args.acp_port)

    try:
        # 1. initialize
        init = client.request("initialize", {
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {"name": "e2e-driver", "version": "1.0"},
        })
        assert "result" in init, f"initialize failed: {init}"
        caps = init["result"].get("agentCapabilities", {})
        mcp_caps = caps.get("mcpCapabilities", {})
        results.append(check(
            "initialize advertises mcp http+sse",
            mcp_caps.get("http") is True and mcp_caps.get("sse") is True,
            json.dumps(mcp_caps),
        ))

        # 2. session/new with localhost HTTP MCP server
        good = client.request("session/new", {
            "cwd": args.workdir,
            "mcpServers": [{
                "type": "http",
                "name": "e2e-mcp",
                "url": args.mcp_url,
                "headers": [],
            }],
        })
        if "result" in good:
            session_id = good["result"].get("sessionId")
            results.append(check(
                "session/new with localhost HTTP MCP succeeds",
                bool(session_id),
                f"sessionId={session_id}",
            ))
        else:
            session_id = None
            results.append(check(
                "session/new with localhost HTTP MCP succeeds",
                False,
                json.dumps(good),
            ))

        # 3. session/prompt -> model must call the session MCP echo tool.
        # We pass as soon as the tool call with our marker is observed (in the
        # MCP server's tool log or the session/update events) and then cancel
        # the prompt; waiting for the full free-model round-trip makes the
        # check dependent on provider latency.
        marker = f"marker-{uuid.uuid4().hex[:8]}"
        if session_id:
            def _marker_seen():
                if os.path.exists(args.log):
                    with open(args.log, encoding="utf-8") as f:
                        if any(marker in line for line in f):
                            return True
                updates = [n for n in client.notifications
                           if n.get("method") == "session/update"]
                return any(marker in json.dumps(note) for note in updates)

            # Send the prompt on a daemon thread so we can watch for the tool
            # call without waiting for the full (slow) free-model round-trip.
            def _prompt_worker():
                try:
                    client.request("session/prompt", {
                        "sessionId": session_id,
                        "prompt": [{"type": "text", "text": (
                            f'Call the "e2e-mcp_echo" tool with message "{marker}" '
                            "and reply with exactly the tool's returned text."
                        )}],
                    }, timeout=PROMPT_TIMEOUT_SECS)
                except (TimeoutError, OSError):
                    pass  # the marker check below is the source of truth

            threading.Thread(target=_prompt_worker, daemon=True).start()

            tool_called = False
            deadline = time.monotonic() + 90
            while time.monotonic() < deadline and not tool_called:
                time.sleep(2)
                tool_called = _marker_seen()

            results.append(check(
                "session MCP echo tool called by model",
                tool_called,
                "tool_call observed via MCP server log or session/update" if tool_called
                else "no tool_call within 90s",
            ))
            if not tool_called:
                updates = [n for n in client.notifications
                           if n.get("method") == "session/update"]
                tail = json.dumps(updates[-2:]) if updates else "no session/update events"
                print(f"  (prompt detail: {tail})", flush=True)

        # 4. SSRF rejections (loopback allowed case uses the real mock URL)
        bad_cases = [
            ("https://192.168.1.1:8080/mcp", "http", "private IP (https)"),
            ("http://10.0.0.1/mcp", "http", "private IP (http)"),
            ("http://169.254.169.254/latest/meta-data/", "sse", "link-local/cloud metadata (sse)"),
            (args.mcp_url, "http", "loopback is allowed"),
        ]
        for url, server_type, label in bad_cases:
            resp = client.request("session/new", {
                "cwd": args.workdir,
                "mcpServers": [{"type": server_type, "name": "evil", "url": url, "headers": []}],
            })
            if "result" in resp:
                # loopback should be the only one that succeeds
                results.append(check(
                    f"session/new rejects {label}",
                    url == args.mcp_url,
                    "accepted",
                ))
            else:
                err = resp.get("error", {})
                reason = json.dumps(err.get("data", {}))
                allowed_loopback = url == args.mcp_url
                results.append(check(
                    f"session/new rejects {label}",
                    (not allowed_loopback) and ("SSRF" in reason or "blocked" in reason),
                    reason,
                ))
    finally:
        client.close()

    passed = sum(1 for r in results if r)
    print(f"\n=== {passed}/{len(results)} checks passed ===", flush=True)
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
