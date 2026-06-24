# Connect CodeWeave to Perplexity

CodeWeave is added to supported Perplexity products as a custom **Connector** using a public HTTPS MCP URL.

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

1. Open Perplexity’s Connector settings.
2. Add a custom Connector where that option is supported.
3. Enter the public CodeWeave `/mcp` URL.
4. Name the Connector `CodeWeave`.
5. Enable it for the relevant space or conversation.

Connector availability and labels can depend on the selected Perplexity product, plan, organization policy, and current release.

## Authentication

Do not enter `.mcp-token` in Perplexity. It is an internal origin credential injected by the tunnel or trusted reverse proxy.

Perplexity only receives the public HTTPS Connector URL.

## Verify safely

Use a disposable repository first. Confirm read-only workspace summary, search, and fetch behavior before enabling edits, Git operations, or command execution.
