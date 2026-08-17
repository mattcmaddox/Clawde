# ACP (Agent Client Protocol) Server

Clawde implements the [Agent Client Protocol](https://agentclientprotocol.com) (ACP) —
a standardized JSON-RPC 2.0 protocol for communication between AI coding agents and
editors. In addition to the standard stdio mode (used by editors like Zed, Neovim,
VS Code), Clawde supports **TCP mode**. The safe default for personal integrations
is localhost-only access; LAN access is an explicit deployment choice because ACP
clients can use Clawde's providers, keys, and tools.

## Modes

### 1. Standalone localhost server

Run a dedicated server process (no TUI) for apps on this machine:

```bash
clawde acp --listen 127.0.0.1:9876
```

This starts the ACP server in TCP mode, accepting local connections on port
`9876`. The process runs headlessly — no interactive TUI is shown. ACP has no
application-level authentication, so do not replace `127.0.0.1` with
`0.0.0.0` unless you add an authenticated, protected transport.

The configured local-Ollama service uses a separate settings profile:

```text
CLAWDE_HOME=~/.clawde/ollama-acp
provider: ollama
model: deepseek-coder:latest
api_base: http://127.0.0.1:11434/v1
mode: isolated + allow_local_host: true
ACP: 127.0.0.1:9876
```

Your normal `~/.clawde` profile remains independent and continues to default to
Free Mode.

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

## Connecting from local apps

Any ACP-compatible app on the same machine can send JSON-RPC requests to `127.0.0.1:9876` using tools
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
}' | socat - TCP:127.0.0.1:9876
```

A successful response
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
      "mcpCapabilities": { "http": true, "sse": true }
    },
    "agentInfo": {
      "name": "clawde",
      "title": "Clawde",
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
    "listen": "127.0.0.1:9876",
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

## Session MCP

`session/new` may include a session-owned `mcpServers` roster. Clawde supports
these ACP server types:

- `stdio` — launches an isolated child process. The command must be an absolute
  path; argument, environment-variable, value-size, duplicate-name, and server
  count limits are enforced.
- `http` — connects to a streamable-HTTP MCP endpoint.
- `sse` — connects to a legacy SSE MCP endpoint.

Session MCP connections and tool bindings belong to that ACP session rather than
the process-wide global MCP manager. Removing or shutting down a session stops
its notification tasks and closes its session-owned MCP resources. A failed
multi-server initialization is transactional: previously opened session
connections are torn down before the session is rejected.

### Remote session-MCP security

Remote HTTP/SSE URLs supplied by an ACP client pass two layers of validation:

1. **Before connection:** malformed URLs, unsupported schemes, private and
   reserved IPv4/IPv6 ranges, loopback, link-local/cloud-metadata addresses, and
   non-HTTPS production URLs are rejected. Plain HTTP is allowed only for local
   development endpoints.
2. **At connection time:** DNS answers are resolved and checked before reqwest
   connects, so the transport uses only validated addresses. Every redirect hop
   is checked again and redirect chains are capped. This protects against DNS
   rebinding and redirect-based SSRF bypasses.

Client-provided ACP HTTP headers are intentionally not forwarded to remote MCP
servers. This prevents token passthrough from an ACP client; authentication must
come through Clawde's existing validated credential/OAuth flow. Per-process
credential storage is not a substitute for ACP authorization, so do not expose
an unauthenticated ACP listener to untrusted clients.

### General ACP security

- **No authentication** — ACP v1 has no token/credential field in its
  `authenticate` method. Bind to `127.0.0.1` for local-only access.
- **LAN access is not the default** — If you intentionally bind to a LAN address,
  use TLS plus an authenticated tunnel or reverse proxy. TLS alone does not add
  ACP authorization.
- **Process-level sharing** — Every connected client shares the ACP process's
  provider registry, settings profile, API access, and global tools. Session MCP
  tool bindings are isolated by session, but credentials and process resources
  remain process-scoped. Use a dedicated `CLAWDE_HOME` profile when an external
  app should use local Ollama rather than your normal Free Mode credentials.

## ACP Protocol Methods

| Method                       | Direction | Notes                                       |
|------------------------------|-----------|---------------------------------------------|
| `initialize`                 | Client → Server | Capability negotiation                  |
| `authenticate`               | Client → Server | No-op (transport-level only)            |
| `session/new`                | Client → Server | Create a session with cwd + session-owned MCP roster |
| `session/prompt`             | Client → Server | Run a turn; streams `session/update`    |
| `session/cancel`             | Client → Server | Cancel an in-flight prompt              |
| `session/update`             | Server → Client | Streamed text/tool deltas               |
| `session/request_permission` | Server → Client | Tool approval dialog                    |
