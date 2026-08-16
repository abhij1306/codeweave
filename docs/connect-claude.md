# Connect CodeWeave to Claude

CodeWeave is added to Claude as a custom **Connector** using a public HTTPS MCP URL.

## Start CodeWeave and a tunnel

Terminal 1:

```bash
codeweave serve --transport http --config config.json
```

Terminal 2 starts an MCP gateway or HTTPS reverse proxy that authenticates the
external Claude caller before adding the private CodeWeave origin bearer.

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

Do not enter `.mcp-token` in Claude. It is a private hop credential used between
the authenticated gateway and the local CodeWeave server.

The public endpoint must require a caller identity supported by your Claude
deployment. A public URL by itself is never sufficient. The bundled
`start-ngrok.ps1` helper requires HTTP Basic authentication; use it only when
the selected Claude client can attach those credentials. Otherwise use an
OAuth-capable MCP gateway.

## Verify safely

Claude sees the same fixed 25-tool surface as every other client. Multiple clients
connected to one CodeWeave process share workspace and Bash state.
