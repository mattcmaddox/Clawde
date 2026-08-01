#!/usr/bin/env python3
"""
acp-client.py — ACP LAN client for daily use.

Chat with a Clawde ACP server over your LAN. No pip install needed —
this is pure Python stdlib.

Usage:
  # Single prompt (streams output, then exits)
  acp-client.py 192.168.1.55 "write a fibonacci function in python"

  # Read prompt from stdin (pipe mode)
  echo "list all files" | acp-client.py 192.168.1.55

  # Interactive REPL (maintains conversation context)
  acp-client.py 192.168.1.55
  acp-client.py 192.168.1.55 --interactive

  # TLS options
  acp-client.py 192.168.1.55 --no-tls              # plain TCP
  acp-client.py 192.168.1.55 --cert server.crt      # verify server cert

  # Custom port
  acp-client.py 192.168.1.55 "hello" --port 9877
"""

import sys
import os
import json
import ssl
import socket
import argparse
import shutil

# ── ANSI helpers ──────────────────────────────────────────────────────────────

COLORS = {
    "dim": "\x1b[2m",
    "italic": "\x1b[3m",
    "green": "\x1b[32m",
    "yellow": "\x1b[33m",
    "blue": "\x1b[34m",
    "bold": "\x1b[1m",
    "reset": "\x1b[0m",
}
USE_COLOR = sys.stdout.isatty()


def c(code, text):
    """Color a string if stdout is a TTY."""
    if USE_COLOR:
        return f"{COLORS[code]}{text}{COLORS['reset']}"
    return text


def dim(text):
    return c("dim", text)


def green(text):
    return c("green", text)


def yellow(text):
    return c("yellow", text)


def bold(text):
    return c("bold", text)


# ── ACP wire protocol ─────────────────────────────────────────────────────────

class AcpClient:
    """Low-level ACP client — connect, init, session, prompt."""

    def __init__(self, host, port, use_tls=True, cert_path=None):
        self.host = host
        self.port = port
        self.sock = self._connect(host, port, use_tls, cert_path)
        self._buf = b""  # read buffer for streaming
        self.msg_id = 0
        self.session_id = None

    @staticmethod
    def _connect(host, port, use_tls, cert_path):
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(60)
        if use_tls:
            ctx = ssl.create_default_context()
            ctx.check_hostname = False
            if cert_path:
                ctx.load_verify_locations(cert_path)
                ctx.verify_mode = ssl.CERT_REQUIRED
            else:
                ctx.verify_mode = ssl.CERT_NONE
            sock = ctx.wrap_socket(sock, server_hostname=host)
        sock.connect((host, port))
        return sock

    def _send(self, method, params):
        """Send a JSON-RPC request and return the ID."""
        self.msg_id += 1
        msg = {
            "jsonrpc": "2.0",
            "id": self.msg_id,
            "method": method,
            "params": params,
        }
        self.sock.sendall(json.dumps(msg, ensure_ascii=False).encode("utf-8") + b"\n")
        return self.msg_id

    def _recv(self):
        """Read the next JSON line (response or notification).

        Uses a manual buffer instead of makefile() to avoid buffering
        issues with SSL sockets.
        """
        while b"\n" not in self._buf:
            chunk = self.sock.recv(4096)
            if not chunk:
                raise ConnectionError("Server closed the connection")
            self._buf += chunk
        line, self._buf = self._buf.split(b"\n", 1)
        return json.loads(line.decode("utf-8").rstrip("\r"))

    def initialize(self):
        """Send initialize and return agent info."""
        self._send("initialize", {
            "protocolVersion": "v1",
            "clientInfo": {"name": "acp-client", "version": "1.0"},
            "clientCapabilities": {},
        })
        resp = self._recv()
        return resp.get("result", {}).get("agentInfo", {})

    def create_session(self):
        """Create a new conversation session."""
        self._send("session/new", {"cwd": os.getcwd(), "mcpServers": []})
        resp = self._recv()
        self.session_id = resp.get("result", {}).get("sessionId")
        return self.session_id

    def prompt(self, text, on_text=None):
        """Send a prompt and stream the response.

        Yields (kind, data) tuples where kind is "text", "thought", "tool_call",
        or "done" (yielded once with stop_reason as data).

        If *on_text* is provided, text and tool_call chunks are written to it
        in real time (useful for streaming to stdout).
        """
        msg_id = self._send("session/prompt", {
            "sessionId": self.session_id,
            "prompt": [{"type": "text", "text": text}],
        })

        while True:
            msg = self._recv()

            if msg.get("method") == "session/update":
                update = msg.get("params", {}).get("update", {})
                utype = update.get("sessionUpdate")
                content = update.get("content", {})

                if utype == "agent_message_chunk":
                    chunk = content.get("text", "") if isinstance(content, dict) else ""
                    if on_text:
                        on_text(chunk)
                    yield "text", chunk

                elif utype == "agent_thought_chunk":
                    chunk = content.get("text", "") if isinstance(content, dict) else ""
                    yield "thought", chunk

                elif utype == "tool_call":
                    title = update.get("title", "") or update.get("name", "?")
                    if on_text:
                        on_text(f"\n[{title}]")
                    yield "tool_call", title

            if msg.get("id") == msg_id and "result" in msg:
                stop_reason = msg["result"].get("stopReason", "unknown")
                yield "done", stop_reason
                return

    def close(self):
        self.sock.close()


# ── CLI ───────────────────────────────────────────────────────────────────────

def run_single(client, prompt_text):
    """One-shot mode: send prompt, stream output, print stop reason."""
    response_text = []
    had_thought = False

    for kind, data in client.prompt(
        prompt_text,
        on_text=lambda t: (sys.stdout.write(t), sys.stdout.flush()),
    ):
        if kind == "thought" and data:
            if not had_thought:
                sys.stdout.write(dim("..."))
                sys.stdout.flush()
                had_thought = True
        elif kind == "text":
            response_text.append(data)
        elif kind == "done":
            print()
            print(dim(f"[{data}]"))

    sys.stdout.flush()
    return "".join(response_text)


def run_interactive(client):
    """Interactive REPL mode — one session, multiple prompts."""
    term_width = shutil.get_terminal_size((80, 20)).columns
    print("─" * term_width)
    print(f"  {bold('Clawde ACP')}  —  {client.host}:{client.port}  —  "
          f"Type your prompt, {dim('Ctrl+D')} to exit, {dim('Ctrl+C')} to cancel")
    print("─" * term_width)

    while True:
        try:
            sys.stdout.write("\n" + green(">>> "))
            sys.stdout.flush()
            line = sys.stdin.readline()
        except (EOFError, KeyboardInterrupt):
            print()
            break

        if not line:
            break

        text = line.rstrip("\n").strip()
        if not text:
            continue

        print(dim("── response ──"))
        for kind, data in client.prompt(text):
            if kind == "text":
                sys.stdout.write(data)
                sys.stdout.flush()
            elif kind == "thought" and data:
                pass  # don't show thought chunks in interactive mode
            elif kind == "done":
                print()
                print(dim(f"  [{data}]"))
        sys.stdout.flush()


def main():
    parser = argparse.ArgumentParser(
        description="ACP LAN client — chat with Clawde over the LAN",
        epilog=(
            "Examples:\n"
            "  acp-client.py 192.168.1.55 \"explain how DNS works\"\n"
            "  acp-client.py 192.168.1.55 --interactive\n"
            "  echo \"hello\" | acp-client.py 192.168.1.55\n"
            "  acp-client.py 192.168.1.55 --no-tls --port 9876"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("host", help="ACP server IP or hostname")
    parser.add_argument("prompt", nargs="?", help="Prompt to send (omit for pipe or interactive mode)")
    parser.add_argument("--port", "-p", type=int, default=9876, help="ACP port (default: 9876)")
    parser.add_argument("--no-tls", action="store_true", help="Use plain TCP instead of TLS")
    parser.add_argument("--cert", metavar="FILE", help="CA cert for TLS verification")
    parser.add_argument("--interactive", "-i", action="store_true", help="Interactive REPL mode")

    args = parser.parse_args()

    # Determine mode
    has_prompt = args.prompt is not None
    is_pipe = not sys.stdin.isatty()
    interactive = args.interactive or (not has_prompt and not is_pipe)

    # Connect
    use_tls = not args.no_tls
    try:
        client = AcpClient(args.host, args.port, use_tls, args.cert)
    except (socket.timeout, ConnectionRefusedError, ssl.SSLError) as e:
        print(f"{dim('error:')} {e}", file=sys.stderr)
        sys.exit(1)

    try:
        # Initialize
        agent = client.initialize()
        name = agent.get("name", "clawde")
        version = agent.get("version", "?")
        if not interactive and not has_prompt and not is_pipe:
            # Server info only
            print(f"Connected to {bold(name)} v{version} @ {args.host}:{args.port}")
            client.close()
            return

        # Create session
        sid = client.create_session()
        if not sid:
            print(f"{dim('error:')} failed to create session", file=sys.stderr)
            sys.exit(1)

        # Extend timeout for long-running prompts (tool execution,
        # FreeProvider fallback chains, etc. can take minutes)
        client.sock.settimeout(300)

        if interactive:
            run_interactive(client)
        elif is_pipe:
            prompt_text = sys.stdin.read()
            run_single(client, prompt_text)
        else:
            run_single(client, args.prompt)

    except (ConnectionError, BrokenPipeError, socket.timeout) as e:
        print(f"\n{dim('connection lost:')} {e}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        print()
    finally:
        client.close()


if __name__ == "__main__":
    main()
