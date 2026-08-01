# ACP (Agent Client Protocol) Server

Clawde implements the [Agent Client Protocol](https://agentclientprotocol.com) (ACP) —
a standardized JSON-RPC 2.0 protocol for communication between AI coding agents and
editors. In addition to the standard stdio mode (used by editors like Zed, Neovim,
VS Code), Clawde supports **TCP mode** for LAN access, allowing other machines on
your network to connect to your Clawde instance and use its providers, keys, and
MCP tools.

## Modes

### 1. Standalone TCP Server

Run a dedicated server process (no TUI):

```bash
clawde acp --listen 0.0.0.0:9876
```

This starts the ACP server in TCP mode, accepting connections on port `9876`.
The process runs headlessly — no interactive TUI is shown.

### 2. Embedded Server (alongside TUI)

Configure the ACP server to start automatically when you run Clawde interactively.
Add to `~/.clawde/settings.json`:

```json
{
  "acpServer": {
    "enabled": true,
    "listen": "127.0.0.1:9876"
  }
}
```

Now every `clawde` session also serves ACP connections in the background. Tokio
cancels the server task on shutdown automatically.

## Connecting from LAN Clients

Any machine on your network can send JSON-RPC requests to the server using tools
like `socat`, `nc`, or any ACP-compatible client (Zed, VS Code extension, etc.).

**Test the connection:**

```bash
echo '{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "v1",
    "clientInfo": { "name": "my-client", "version": "1.0" },
    "clientCapabilities": {}
  }
}' | socat - TCP:192.168.1.100:9876
```

Replace `192.168.1.100` with your server's LAN IP address. A successful response
looks like:

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": 1,
    "agentCapabilities": {
      "loadSession": false,
      "promptCapabilities": {},
      "mcpCapabilities": {}
    },
    "agentInfo": {
      "name": "clawde",
      "title": "Claurst",
      "version": "0.1.8"
    }
  }
}
```

## TLS (Optional)

For encrypted connections on untrusted networks, configure TLS certificates.
Add to `~/.clawde/settings.json`:

```json
{
  "acpServer": {
    "enabled": true,
    "listen": "0.0.0.0:9876",
    "tlsCertPath": "/path/to/cert.pem",
    "tlsKeyPath": "/path/to/key.pem"
  }
}
```

When both `tlsCertPath` and `tlsKeyPath` are set, the server wraps every
connection with TLS via rustls. Supported private key formats:
- PKCS#8 (`BEGIN PRIVATE KEY`)
- EC / SEC1 (`BEGIN EC PRIVATE KEY`)
- RSA (`BEGIN RSA PRIVATE KEY`)

If only one of the two paths is configured, the server logs a warning and falls
back to plain TCP.

### Generating self-signed certificates for testing

```bash
# Generate a self-signed cert and key
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem \
  -days 365 -nodes -subj "/CN=clawde-acp"
```

## Security Considerations

- **No authentication** — ACP v1 has no token/credential field in its
  `authenticate` method. Bind to `127.0.0.1` (the default) for local-only access,
  or use an SSH tunnel / VPN for remote access over untrusted networks.
- **TLS recommended** — Use `tlsCertPath` / `tlsKeyPath` for encrypted connections
  when binding to a network-accessible address.
- **Process-level sharing** — All LAN clients share the same provider registry,
  API keys, and MCP tools as the host Clawde instance.

## ACP Protocol Methods

| Method                       | Direction | Notes                                       |
|------------------------------|-----------|---------------------------------------------|
| `initialize`                 | Client → Server | Capability negotiation                  |
| `authenticate`               | Client → Server | No-op (transport-level only)            |
| `session/new`                | Client → Server | Create a session with cwd               |
| `session/prompt`             | Client → Server | Run a turn; streams `session/update`    |
| `session/cancel`             | Client → Server | Cancel an in-flight prompt              |
| `session/update`             | Server → Client | Streamed text/tool deltas               |
| `session/request_permission` | Server → Client | Tool approval dialog                    |
