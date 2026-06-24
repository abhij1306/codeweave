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

CodeWeave is powerful local development tooling. A connected MCP client may be able to read and modify code, invoke approved commands, and perform bounded Git operations.

For safer operation:

- prefer stdio for local clients;
- bind HTTP to `127.0.0.1` unless remote access is explicitly required;
- keep bearer authentication enabled;
- expose remote HTTP only through trusted HTTPS infrastructure;
- restrict `workspace.allowedRoots` to the smallest useful directories;
- leave shell execution disabled;
- minimize `policy.allowedCommands`;
- protect the token file and local configuration;
- run under a dedicated, least-privileged operating-system account when practical;
- test upgrades against a disposable repository;
- review diffs before committing or pushing changes.

CodeWeave does not make an untrusted model, prompt, extension, or MCP client safe. Only connect clients you trust and review their requested operations.
