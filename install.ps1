<#
.SYNOPSIS
    Cryo Vault installer for Windows (PowerShell 5.1+ / PowerShell Core).

.DESCRIPTION
    Installs the `cryo` CLI (and `cryo-vault-mcp` MCP server) into
    %USERPROFILE%\.cryo-vault\bin and ensures that directory is on the user's
    PATH so that typing `cryo` in a fresh terminal Just Works.

    Sources, in order:
      1) Local dist\ folder if this script is run from inside the repo.
      2) GitHub releases for the requested version otherwise.

    Removes any prior install it finds (older versions, stale shims,
    previous PATH entry) before laying down the new one.

.PARAMETER Version
    Version to install (default: v0.2.0). Accepts "v0.2.0" or "0.2.0".

.PARAMETER Prefix
    Install prefix. Binaries go to <Prefix>\bin. Default: $env:USERPROFILE\.cryo-vault

.PARAMETER Source
    Force binary source: 'local' or 'github'. Default: auto.

.PARAMETER Uninstall
    Remove any installed cryo-vault and clean PATH entry.

.PARAMETER NoPath
    Don't modify the user PATH environment variable.

.PARAMETER Force
    Reinstall even if the same version is already present.

.EXAMPLE
    .\install.ps1
.EXAMPLE
    .\install.ps1 -Version v0.2.0 -Force
.EXAMPLE
    .\install.ps1 -Uninstall
#>

[CmdletBinding()]
param(
    [string]$Version = "v0.2.0",
    [string]$Prefix  = (Join-Path $env:USERPROFILE ".cryo-vault"),
    [ValidateSet("local","github","auto")]
    [string]$Source  = "auto",
    [switch]$Uninstall,
    [switch]$NoPath,
    [switch]$Force
)

$ErrorActionPreference = "Stop"

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

$GithubRepo       = "aghontpi/cryo-vault"
$ScriptDir        = Split-Path -Parent $MyInvocation.MyCommand.Path
$PathMarkerLegacy = "# cryo-vault"  # in case a future shell rc edit appears
if (-not $Version.StartsWith("v")) { $Version = "v$Version" }
$BinDir       = Join-Path $Prefix "bin"
$VersionsDir  = Join-Path $Prefix "versions"
$InstallDir   = Join-Path $VersionsDir $Version
$VersionFile  = Join-Path $Prefix ".version"

# ---------------------------------------------------------------------------
# Pretty output
# ---------------------------------------------------------------------------

function Write-Info ($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok   ($msg) { Write-Host "[ok] $msg" -ForegroundColor Green }
function Write-Warn ($msg) { Write-Host "[!]  $msg" -ForegroundColor Yellow }
function Write-Err  ($msg) { Write-Host "[x]  $msg" -ForegroundColor Red }

function Fail ($msg) {
    Write-Err $msg
    exit 1
}

# ---------------------------------------------------------------------------
# Platform detection
# ---------------------------------------------------------------------------

function Get-Platform {
    $archEnv = $env:PROCESSOR_ARCHITECTURE
    # On 32-bit PowerShell hosted under 64-bit Windows, PROCESSOR_ARCHITEW6432
    # carries the real arch.
    if ($env:PROCESSOR_ARCHITEW6432) { $archEnv = $env:PROCESSOR_ARCHITEW6432 }
    switch -Regex ($archEnv) {
        '^(AMD64|x86_64)$' { return "windows-x64" }
        '^(ARM64|AARCH64)$' { return "windows-arm64" }
        default { Fail "Unsupported architecture: $archEnv" }
    }
}

# ---------------------------------------------------------------------------
# Source selection
# ---------------------------------------------------------------------------

function Resolve-Source ($Platform) {
    $localCli = Join-Path $ScriptDir "dist\cryo-vault-$Version-$Platform.exe"
    $localMcp = Join-Path $ScriptDir "dist\cryo-vault-mcp-$Version-$Platform.exe"

    switch ($Source) {
        "local" {
            if (-not (Test-Path $localCli)) { Fail "Local source requested but not found: $localCli" }
            if (-not (Test-Path $localMcp)) { Fail "Local source requested but not found: $localMcp" }
            return @{ Kind = "local"; Cli = $localCli; Mcp = $localMcp }
        }
        "github" {
            return @{ Kind = "github"; Cli = $null; Mcp = $null }
        }
        "auto" {
            if ((Test-Path $localCli) -and (Test-Path $localMcp)) {
                return @{ Kind = "local"; Cli = $localCli; Mcp = $localMcp }
            } else {
                return @{ Kind = "github"; Cli = $null; Mcp = $null }
            }
        }
    }
}

# ---------------------------------------------------------------------------
# Stray detection
# ---------------------------------------------------------------------------

function Get-StrayCryoOnPath {
    $managed = Join-Path $BinDir "cryo.exe"
    $hits = Get-Command cryo -All -ErrorAction SilentlyContinue
    if (-not $hits) { return @() }
    $stray = @()
    foreach ($h in $hits) {
        $src = $h.Source
        if ($src -and ($src -ine $managed)) { $stray += $src }
    }
    return $stray
}

# ---------------------------------------------------------------------------
# PATH wiring (User-scope environment variable)
# ---------------------------------------------------------------------------

function Get-UserPathArray {
    $current = [Environment]::GetEnvironmentVariable("Path", "User")
    if ([string]::IsNullOrEmpty($current)) { return @() }
    return $current -split ';' | Where-Object { $_ -ne "" }
}

function Set-UserPathArray ($arr) {
    $joined = ($arr -join ';')
    [Environment]::SetEnvironmentVariable("Path", $joined, "User")
}

function Add-BinDirToUserPath {
    $parts = Get-UserPathArray
    if ($parts -contains $BinDir) {
        Write-Ok "User PATH already contains $BinDir"
        return
    }
    # Prepend so we shadow any stray older install.
    Set-UserPathArray (@($BinDir) + $parts)
    Write-Ok "Added $BinDir to user PATH"
}

function Remove-BinDirFromUserPath {
    $parts = Get-UserPathArray
    $filtered = $parts | Where-Object { $_ -ne $BinDir }
    if (@($filtered).Count -eq @($parts).Count) {
        Write-Warn "User PATH did not contain $BinDir (nothing to remove)"
        return
    }
    Set-UserPathArray $filtered
    Write-Ok "Removed $BinDir from user PATH"
}

# ---------------------------------------------------------------------------
# Uninstall
# ---------------------------------------------------------------------------

function Invoke-Uninstall {
    Write-Info "Uninstalling cryo-vault from $Prefix"
    if (Test-Path $Prefix) {
        Remove-Item -Recurse -Force $Prefix
        Write-Ok "Removed $Prefix"
    } else {
        Write-Warn "No install found at $Prefix"
    }
    if (-not $NoPath) {
        Remove-BinDirFromUserPath
    }
    $strays = Get-StrayCryoOnPath
    if ($strays.Count -gt 0) {
        Write-Warn "Other 'cryo' binaries are still on your PATH (not managed by this installer):"
        $strays | ForEach-Object { Write-Host "    $_" }
        Write-Warn "Remove them manually if you don't want them shadowing future installs."
    }
}

# ---------------------------------------------------------------------------
# Download / stage
# ---------------------------------------------------------------------------

function Get-File ($Url, $OutFile) {
    Write-Info "Downloading $Url"
    # Force TLS 1.2 for older PS hosts.
    try {
        [Net.ServicePointManager]::SecurityProtocol = `
            [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch { }
    Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
}

function Stage-Binaries ($SourceInfo, $Platform) {
    Write-Info "Staging $Version binaries into $InstallDir"
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    $dstCli = Join-Path $InstallDir "cryo-vault.exe"
    $dstMcp = Join-Path $InstallDir "cryo-vault-mcp.exe"

    if ($SourceInfo.Kind -eq "local") {
        Copy-Item $SourceInfo.Cli $dstCli -Force
        Copy-Item $SourceInfo.Mcp $dstMcp -Force
        Write-Ok "Copied local binaries from dist\"
    } else {
        $base = "https://github.com/$GithubRepo/releases/download/$Version"
        Get-File "$base/cryo-vault-$Version-$Platform.exe"     $dstCli
        Get-File "$base/cryo-vault-mcp-$Version-$Platform.exe" $dstMcp
    }

    foreach ($f in @($dstCli, $dstMcp)) {
        if (-not (Test-Path $f)) { Fail "Missing binary after install: $f" }
        if ((Get-Item $f).Length -le 0) { Fail "Downloaded binary is empty: $f" }
    }
}

# ---------------------------------------------------------------------------
# Activate version (shims)
# ---------------------------------------------------------------------------

function Activate-Version {
    New-Item -ItemType Directory -Force -Path $BinDir | Out-Null

    # Windows shells don't follow Unix symlinks reliably and may need admin
    # rights to create them. A tiny .cmd shim is portable and zero-privilege.
    $cli = Join-Path $InstallDir "cryo-vault.exe"
    $mcp = Join-Path $InstallDir "cryo-vault-mcp.exe"

    $shims = @(
        @{ Name = "cryo.cmd";           Target = $cli },
        @{ Name = "cryo-vault.cmd";     Target = $cli },
        @{ Name = "cryo-vault-mcp.cmd"; Target = $mcp }
    )

    foreach ($s in $shims) {
        $path = Join-Path $BinDir $s.Name
        $content = "@echo off`r`n`"$($s.Target)`" %*`r`n"
        Set-Content -LiteralPath $path -Value $content -Encoding ASCII -NoNewline
    }

    Set-Content -LiteralPath $VersionFile -Value $Version -Encoding ASCII
    Write-Ok "Linked $BinDir\cryo.cmd -> $cli"
}

# ---------------------------------------------------------------------------
# Prune old versions
# ---------------------------------------------------------------------------

function Prune-OldVersions {
    if (-not (Test-Path $VersionsDir)) { return }
    $removed = 0
    Get-ChildItem -Directory $VersionsDir | ForEach-Object {
        if ($_.Name -ne $Version) {
            Remove-Item -Recurse -Force $_.FullName
            Write-Ok "Removed old version: $($_.Name)"
            $removed++
        }
    }
    if ($removed -eq 0) {
        Write-Host "    no older versions to clean" -ForegroundColor DarkGray
    }
}

# ---------------------------------------------------------------------------
# Write MCP config snippets the user can paste into their AI client
# ---------------------------------------------------------------------------
#
# We deliberately do NOT auto-edit each AI client's MCP config — paths and
# formats vary (Claude Code: ~\.claude.json, Cursor: ~\.cursor\mcp.json,
# VSCode: user mcp.json with `servers` key not `mcpServers`, Antigravity:
# IDE-managed). A single overwrite of these snippet files under $Prefix is
# idempotent by construction: re-running the installer just rewrites them
# in place, never duplicates.

function Write-McpSnippets {
    $cryoMcpBin   = Join-Path $BinDir "cryo-vault-mcp.exe"
    $defaultDb    = Join-Path $env:USERPROFILE ".cryo"
    $mcpSnippet   = Join-Path $Prefix "mcp-config.snippet.json"
    $vscodeSnippet = Join-Path $Prefix "mcp-config.vscode.snippet.json"

    Write-Info "Writing MCP config snippets"

    # Escape backslashes for valid JSON string literals on Windows paths.
    $cryoMcpBinJson = $cryoMcpBin -replace '\\','\\'
    $defaultDbJson  = $defaultDb  -replace '\\','\\'

    # mcpServers schema — Claude Code, Cursor, Antigravity, Claude Desktop.
    $mcpServersJson = @"
{
  "mcpServers": {
    "cryo-vault": {
      "command": "$cryoMcpBinJson",
      "args": [],
      "env": {
        "CRYO_DB_PATH": "$defaultDbJson"
      }
    }
  }
}
"@
    Set-Content -LiteralPath $mcpSnippet -Value $mcpServersJson -Encoding UTF8
    Write-Ok "Wrote $mcpSnippet"

    # VSCode native MCP uses top-level "servers", not "mcpServers".
    $vscodeJson = @"
{
  "servers": {
    "cryo-vault": {
      "command": "$cryoMcpBinJson",
      "args": [],
      "env": {
        "CRYO_DB_PATH": "$defaultDbJson"
      }
    }
  }
}
"@
    Set-Content -LiteralPath $vscodeSnippet -Value $vscodeJson -Encoding UTF8
    Write-Ok "Wrote $vscodeSnippet"
}

function Show-McpPasteGuide {
    $mcpSnippet    = Join-Path $Prefix "mcp-config.snippet.json"
    $vscodeSnippet = Join-Path $Prefix "mcp-config.vscode.snippet.json"
    $cryoMcpBin    = Join-Path $BinDir "cryo-vault-mcp.exe"

    Write-Host ""
    Write-Host "To wire the MCP server into an AI client, paste the relevant snippet into:" -ForegroundColor White
    Write-Host "    Claude Code   $env:USERPROFILE\.claude.json    (or: claude mcp add cryo-vault `"$cryoMcpBin`" --scope user)"
    Write-Host "    Cursor        $env:USERPROFILE\.cursor\mcp.json"
    Write-Host "    Antigravity   IDE -> Manage MCP Servers -> View raw config"
    Write-Host "    VSCode        Cmd-Shift-P -> MCP: Open User Configuration  (use the vscode snippet — key is `"servers`", not `"mcpServers`")"
    Write-Host ""
    Write-Host "    Snippet:        $mcpSnippet" -ForegroundColor Green
    Write-Host "    VSCode snippet: $vscodeSnippet" -ForegroundColor Green
}

# ---------------------------------------------------------------------------
# Main install
# ---------------------------------------------------------------------------

function Invoke-Install {
    $Platform   = Get-Platform
    $SourceInfo = Resolve-Source -Platform $Platform

    Write-Info "Cryo Vault installer"
    Write-Host "    version:  $Version"
    Write-Host "    platform: $Platform"
    Write-Host "    prefix:   $Prefix"
    Write-Host "    source:   $($SourceInfo.Kind)"

    $prev = $null
    if (Test-Path $VersionFile) {
        $prev = (Get-Content -LiteralPath $VersionFile -Raw).Trim()
    }

    if ($prev) {
        if (($prev -eq $Version) -and (-not $Force)) {
            Write-Ok "Cryo Vault $Version is already installed at $Prefix."
            Write-Warn "Re-run with -Force to reinstall, or -Uninstall to remove."
            return
        }
        Write-Info "Replacing existing install ($prev -> $Version)"
    }

    $strays = Get-StrayCryoOnPath
    if ($strays.Count -gt 0) {
        Write-Warn "Found 'cryo' on PATH outside this installer:"
        $strays | ForEach-Object { Write-Host "    $_" }
        Write-Warn "These will shadow the new install unless removed."
    }

    Stage-Binaries -SourceInfo $SourceInfo -Platform $Platform
    Activate-Version
    Prune-OldVersions
    Write-McpSnippets

    if (-not $NoPath) {
        Write-Info "Wiring PATH (user scope)"
        Add-BinDirToUserPath
    } else {
        Write-Warn "Skipping PATH edit (-NoPath). Add manually:"
        Write-Host "    setx PATH `"$BinDir;%PATH%`""
    }

    Write-Host ""
    Write-Ok "Done."
    Write-Host "    Run 'cryo --help' to confirm." -ForegroundColor Green
    if (-not $NoPath) {
        Write-Host "    Open a new terminal so the PATH change is picked up."
    }
    Show-McpPasteGuide
}

# ---------------------------------------------------------------------------
# Entry
# ---------------------------------------------------------------------------

if ($Uninstall) {
    Invoke-Uninstall
} else {
    Invoke-Install
}
