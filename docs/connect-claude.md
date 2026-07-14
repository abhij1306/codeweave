# Connect CodeWeave to Claude

CodeWeave is added to Claude as a custom **Connector** using a public HTTPS MCP URL.

## Start CodeWeave and a tunnel

Terminal 1:

```bash
cargo run --release -- --transport http --config config.json
```

Terminal 2 starts ngrok, Cloudflare Tunnel, or another trusted HTTPS reverse proxy. Follow the commands in the root [README](../README.md#4-expose-codeweave-over-https).

Use the public URL ending in `/mcp`:

```text
https://example.ngrok.app/mcp
```

## Add the Connector

1. Open Claude’s Connector settings or Connector directory.
2. Choose the option to add a custom Connector.
3. Enter the public CodeWeave `/mcp` URL.
4. Name the Connector `CodeWeave`.
5. Enable it for the conversation or workspace where it is needed.

The exact menu names can vary by Claude product and release.

## Authentication

Do not enter `.mcp-token` in Claude. It is an internal origin credential used between the tunnel or reverse proxy and the local CodeWeave server.

Claude only receives the public HTTPS Connector URL.

## Verify safely

Claude sees the same fixed 25-tool surface as every other client. Multiple clients
connected to one CodeWeave process share workspace and Bash state.
