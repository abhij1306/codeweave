# Connect CodeWeave to ChatGPT

CodeWeave is added to ChatGPT through the **Apps** interface using a public HTTPS MCP URL.

## Before connecting

Custom Apps in [Developer Mode](https://developers.openai.com/api/docs/guides/developer-mode)
are available on ChatGPT Pro, Plus, Business, Enterprise, and Education plans
on the web. In a managed workspace, you must be a workspace admin or have the
RBAC permission to create custom Apps. Enable **Developer Mode** under
**Settings → Security and login** before continuing.

ChatGPT custom Apps support [OAuth 2.1](https://developers.openai.com/plugins/build/auth),
no authentication, and mixed authentication. This setup uses **OAuth 2.1** at
the public MCP gateway; the CodeWeave bearer remains a private credential only
for the gateway-to-origin hop.

Start CodeWeave locally:

```bash
codeweave serve --transport http --config config.json
```

Then expose it through an MCP gateway or HTTPS reverse proxy that authenticates
the external ChatGPT caller before it adds the private CodeWeave origin bearer.

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

Do not paste `.mcp-token` into ChatGPT. It is a private hop credential for the
local origin, not external caller authentication.

The public endpoint must require a caller identity supported by your ChatGPT
workspace (normally an OAuth-capable MCP gateway) and inject the origin bearer
only after that identity is accepted. A public URL by itself is never sufficient.

`start-ngrok.ps1` now enforces HTTP Basic authentication and is suitable only
for MCP clients that can send Basic credentials. Do not use it as a URL-only
ChatGPT App endpoint.

## Verify safely

Use a disposable repository first. Ask ChatGPT to:

1. summarize the active workspace;
2. search for a known file;
3. fetch a small read-only range;
4. show Git status.

Review all edits and commands until the deployment is trusted.
