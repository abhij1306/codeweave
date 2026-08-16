param(
    [string]$Config = ".\config.json",
    [string]$Domain = "",
    [Parameter(Mandatory = $true)]
    [System.Management.Automation.PSCredential]$ClientCredential
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

$ClientUsername = $ClientCredential.UserName
$ClientPassword = $ClientCredential.GetNetworkCredential().Password
if ([string]::IsNullOrWhiteSpace($ClientUsername) -or $ClientUsername.Contains(":")) {
    throw "The public tunnel username must be non-empty and cannot contain a colon."
}
if ($ClientPassword.Length -lt 8 -or $ClientPassword.Length -gt 128) {
    throw "The public tunnel password must contain 8 to 128 characters."
}
foreach ($InputValue in @(
    @{ Name = "public tunnel username"; Value = $ClientUsername },
    @{ Name = "public tunnel password"; Value = $ClientPassword },
    @{ Name = "bearer token"; Value = $Token }
)) {
    if ($InputValue.Value.Contains('${')) {
        throw "$($InputValue.Name) must not contain the literal `${ sequence."
    }
}

function ConvertTo-YamlSingleQuoted([string]$Value) {
    return "'" + $Value.Replace("'", "''") + "'"
}

$ExternalCredential = ConvertTo-YamlSingleQuoted "$ClientUsername`:$ClientPassword"
$OriginAuthorization = ConvertTo-YamlSingleQuoted "Bearer $Token"
$Origin = ConvertTo-YamlSingleQuoted "http://127.0.0.1:$Port"

$PolicyPath = Join-Path $env:TEMP "codeweave-ngrok-policy-$PID.yml"

@"
on_http_request:
  - actions:
      - type: basic-auth
        config:
          realm: "CodeWeave MCP"
          credentials:
            - $ExternalCredential
          enforce: true
      - type: remove-headers
        config:
          headers:
            - authorization
            - origin
      - type: add-headers
        config:
          headers:
            authorization: $OriginAuthorization
            origin: $Origin
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
Write-Host "Public auth: HTTP Basic for $ClientUsername (required before origin credential injection)"
$AllowedHosts = @($Settings.server.allowedHosts) | ForEach-Object { "$_".Trim() }
$NgrokHost = $Domain.Trim() -replace '^https?://', ''
$NgrokHost = $NgrokHost.TrimEnd('/')
$HostAllowed = $AllowedHosts -contains "*"
if (-not $HostAllowed -and -not [string]::IsNullOrWhiteSpace($NgrokHost)) {
    $HostAllowed = ($AllowedHosts -contains $NgrokHost) -or ($AllowedHosts -contains "${NgrokHost}:443")
}
if (-not $HostAllowed) {
    if ([string]::IsNullOrWhiteSpace($NgrokHost)) {
        Write-Warning "Random ngrok URLs can return 403 unless server.allowedHosts is [""*""]. Restart CodeWeave after changing the config."
    }
    else {
        Write-Warning "If MCP requests return 403, add ""$NgrokHost"" to server.allowedHosts or use [""*""] for trusted tunnel hosts, then restart CodeWeave."
    }
}
Write-Host "Use the HTTPS forwarding URL shown below, append /mcp, and configure the client with the Basic credential." -ForegroundColor Green

try {
    & ngrok @Arguments
}
finally {
    Remove-Item $PolicyPath -Force -ErrorAction SilentlyContinue
}
