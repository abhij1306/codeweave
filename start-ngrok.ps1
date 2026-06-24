param(
    [string]$Config = ".\config.json",
    [string]$Domain = ""
)

$ErrorActionPreference = "Stop"

if (-not (Get-Command ngrok -ErrorAction SilentlyContinue)) {
    throw "ngrok is not installed or is not available in PATH."
}

$ProjectDir = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $ProjectDir

if (-not (Test-Path $Config)) {
    throw "Configuration file not found: $Config"
}

$ResolvedConfig = (Resolve-Path $Config).Path
$Settings = Get-Content $ResolvedConfig -Raw | ConvertFrom-Json

if ($Settings.server.authMode -ne "bearer") {
    throw "This helper requires server.authMode to be 'bearer'."
}

$Port = [int]$Settings.server.port
if (-not $Port) {
    $Port = 8820
}

$TokenFile = $Settings.server.tokenFile
if ([string]::IsNullOrWhiteSpace($TokenFile)) {
    $TokenFile = ".mcp-token"
}

$ConfigDirectory = Split-Path -Parent $ResolvedConfig
$TokenPath = Join-Path $ConfigDirectory $TokenFile

if (-not (Test-Path $TokenPath)) {
    throw "Bearer token file not found: $TokenPath. Start CodeWeave once so it can generate the token."
}

$Token = (Get-Content $TokenPath -Raw).Trim()
if ([string]::IsNullOrWhiteSpace($Token)) {
    throw "Bearer token file is empty: $TokenPath"
}

$PolicyPath = Join-Path $env:TEMP "codeweave-ngrok-policy-$PID.yml"

@"
on_http_request:
  - actions:
      - type: remove-headers
        config:
          headers:
            - authorization
            - origin
      - type: add-headers
        config:
          headers:
            authorization: "Bearer $Token"
            origin: "http://127.0.0.1:$Port"
            host: "127.0.0.1:$Port"
"@ | Set-Content -Path $PolicyPath -Encoding utf8

$Arguments = @(
    "http",
    "http://127.0.0.1:$Port",
    "--traffic-policy-file", $PolicyPath,
    "--inspect=true"
)

if (-not [string]::IsNullOrWhiteSpace($Domain)) {
    $Arguments += @("--url", "https://$Domain")
}

Write-Host "Starting CodeWeave ngrok tunnel" -ForegroundColor Cyan
Write-Host "Local MCP:  http://127.0.0.1:$Port/mcp"
Write-Host "Inspector:  http://127.0.0.1:4040"
Write-Host "Auth header: injected internally by ngrok"
Write-Host "Use the HTTPS forwarding URL shown below and append /mcp." -ForegroundColor Green

try {
    & ngrok @Arguments
}
finally {
    Remove-Item $PolicyPath -Force -ErrorAction SilentlyContinue
}
