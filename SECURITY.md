# Security Policy

## Supported versions

CodeWeave is currently pre-1.0. Security fixes are applied to the latest version on the default branch. Older commits and forks may not receive fixes.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use the repository host's private security-advisory feature when available, or contact a maintainer through a private channel listed by the repository owner.

Include:

- affected version or commit;
- operating system and transport;
- reproduction steps;
- expected and actual behavior;
- potential impact;
- any suggested mitigation.

Avoid including real credentials, private source code, or personal data in the report.

## Deployment guidance

CodeWeave is powerful local development tooling. A connected MCP client can read and modify code, execute Bash commands as the CodeWeave operating-system user, and perform Git operations.

For safer operation:

- prefer stdio for local clients;
- bind HTTP to `127.0.0.1` unless remote access is explicitly required;
- keep bearer authentication enabled;
- expose remote HTTP only through trusted HTTPS infrastructure;
- point `workspace.path` at the single repository this instance should serve;
- disable `policy.bash.enabled` when command execution is not required;
- protect the token file and local configuration;
- run under a dedicated, least-privileged operating-system account when practical;
- test upgrades against a disposable repository;
- review diffs before committing or pushing changes.

The configured `workspace.path` constrains file tools and Bash `cwd` resolution. It does not sandbox Bash: commands may read, modify, execute, or transmit anything accessible to the CodeWeave operating-system account. CodeWeave does not make an untrusted model, prompt, extension, or MCP client safe. Only connect clients you trust and review their requested operations.
