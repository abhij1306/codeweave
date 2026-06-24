# Installation

## Requirements

- Git
- Rust 1.80 or newer, installed with `rustup`
- A compatible MCP client
- Optional: an HTTPS tunnel or reverse proxy for remote HTTP clients

## Install Rust

### Windows

1. Install Git for Windows.
2. Install Rust using `rustup-init.exe` from the official Rust website.
3. Accept the stable toolchain.
4. Install the Microsoft C++ build tools if prompted.
5. Open a new PowerShell session.

```powershell
rustc --version
cargo --version
git --version
```

### macOS

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Linux

Install Git and your distribution's build-essential package, then:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## Build CodeWeave

```bash
git clone <repository-url>
cd codeweave
cp config.example.json config.json
cargo build --release
```

PowerShell:

```powershell
Copy-Item config.example.json config.json
cargo build --release
```

## Configure

Edit `config.json` and replace all example paths with valid local paths. Keep `allowedRoots` narrower than your home directory whenever possible.

Unix-style example:

```json
{
  "workspace": {
    "defaultPath": "/home/user/projects/example",
    "allowedRoots": ["/home/user/projects"],
    "artifactPaths": ["artifacts"]
  }
}
```

Windows example:

```json
{
  "workspace": {
    "defaultPath": "D:\\Development\\example",
    "allowedRoots": ["D:\\Development"],
    "artifactPaths": ["artifacts"]
  }
}
```

Do not commit `config.json` or `.mcp-token`.

When bearer authentication is enabled, CodeWeave automatically creates `.mcp-token` on the first run if the configured token file is missing. Existing non-empty token files are reused.

## Run

HTTP:

```bash
cargo run --release -- --transport http --config config.json
```

Stdio:

```bash
cargo run --release -- --transport stdio --config config.json
```

## Update

```bash
git pull --ff-only
cargo test --release
cargo build --release
```

Review configuration and release notes before replacing a running instance.
