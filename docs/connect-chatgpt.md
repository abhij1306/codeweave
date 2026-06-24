# Connect CodeWeave to ChatGPT

CodeWeave is added to ChatGPT through the **Apps** interface using a public HTTPS MCP URL.

## Before connecting

Start CodeWeave locally:

```bash
cargo run --release -- --transport http --config config.json
```

Then expose it with ngrok, Cloudflare Tunnel, or another trusted HTTPS reverse proxy as described in the root [README](../README.md#4-expose-codeweave-over-https).

Your final URL must end in `/mcp`, for example:

```text
https://example.ngrok.app/mcp
```

## Add the App

1. Open ChatGPT settings.
2. Open **Apps**.
3. Add or create an App using the public CodeWeave MCP URL.
4. Name it `CodeWeave`.
5. Enable the App in a chat and verify its tools are available.

The exact labels can vary by account, workspace policy, and current ChatGPT release.

## Authentication

Do not paste `.mcp-token` into ChatGPT. That token protects the local CodeWeave origin and should be injected internally by ngrok or another trusted reverse proxy.

ChatGPT only needs the public HTTPS `/mcp` App URL.

## Verify safely

Use a disposable repository first. Ask ChatGPT to:

1. summarize the active workspace;
2. search for a known file;
3. fetch a small read-only range;
4. show Git status.

Review all edits and commands until the deployment is trusted.
