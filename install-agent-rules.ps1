<#
.SYNOPSIS
    Cryo Vault — agent-rules installer (Windows / PowerShell 5.1+).

.DESCRIPTION
    Drops a short auto-capture instruction snippet into the rule-file of every
    AI client you use, so that Claude Code, GitHub Copilot, Antigravity (and
    any other agent that reads the cross-tool AGENTS.md convention) will
    archive every finished conversation to Cryo Vault automatically.

    Targets:
      $HOME\.claude\CLAUDE.md                 — global, Claude Code
      $HOME\.gemini\AGENTS.md                 — global, Antigravity + cross-tool
      .\.github\copilot-instructions.md       — project-scoped, VSCode Copilot
                                                (run from each repo where you
                                                 want this active; Copilot has
                                                 no clean global rules path)

    Idempotent: re-running this script replaces the previously-written snippet
    in place rather than appending duplicates. Uses HTML-comment markers so
    the rest of each file is left untouched.

.PARAMETER Uninstall
    Remove the snippet from all targets instead of writing it.

.PARAMETER SkipClaude
    Leave $HOME\.claude\CLAUDE.md alone.

.PARAMETER SkipAgents
    Leave $HOME\.gemini\AGENTS.md alone.

.PARAMETER SkipCopilot
    Leave .\.github\copilot-instructions.md alone.

.PARAMETER DryRun
    Print what would change without modifying any files.

.EXAMPLE
    .\install-agent-rules.ps1
.EXAMPLE
    .\install-agent-rules.ps1 -SkipCopilot
.EXAMPLE
    .\install-agent-rules.ps1 -Uninstall
#>

[CmdletBinding()]
param(
    [switch]$Uninstall,
    [switch]$SkipClaude,
    [switch]$SkipAgents,
    [switch]$SkipCopilot,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

# ---------------------------------------------------------------------------
# Config
# ---------------------------------------------------------------------------

$ScriptDir   = Split-Path -Parent $MyInvocation.MyCommand.Path
$SkillPath   = Join-Path $ScriptDir "Skills\auto-capture\SKILL.md"

$MarkerBegin = "<!-- cryo-vault:auto-capture start -->"
$MarkerEnd   = "<!-- cryo-vault:auto-capture end -->"

$ClaudeRules  = Join-Path $HOME ".claude\CLAUDE.md"
$GeminiRules  = Join-Path $HOME ".gemini\AGENTS.md"
$CopilotRules = Join-Path (Get-Location) ".github\copilot-instructions.md"

# ---------------------------------------------------------------------------
# Pretty output
# ---------------------------------------------------------------------------

function Write-Info($msg) { Write-Host "==> $msg" -ForegroundColor Blue }
function Write-Ok($msg)   { Write-Host "[ok] $msg" -ForegroundColor Green }
function Write-Warn($msg) { Write-Host "[!]  $msg" -ForegroundColor Yellow }
function Write-Err($msg)  { Write-Host "[x]  $msg" -ForegroundColor Red; exit 1 }

# ---------------------------------------------------------------------------
# Snippet
# ---------------------------------------------------------------------------

if (-not (Test-Path $SkillPath)) {
    Write-Err "Canonical skill missing: $SkillPath"
}

$SnippetBody = @'
## Cryo Vault — auto-archive conversations

At the end of every conversation, archive the session to Cryo Vault so it
becomes searchable later.

1. **Preferred:** call the `add_log` tool on the `cryo-vault` MCP server.
   The tool description carries the full session schema and the title
   rules — follow them. Nothing else to read.

2. **Fallback (only if the MCP server is not connected):** pipe the
   session JSON to `cryo add -`. Do **not** guess the schema — read
   the `store-conversations` skill or `cryo --help` first.

Always include a `title` of 3–7 words that summarises the session.
Never send placeholders like "Untitled", "Chat", "New chat", or "".

See the full guidance in `Skills/auto-capture/SKILL.md` of the
cryo-vault repo.
'@

$SnippetBlock = "$MarkerBegin`n$SnippetBody`n$MarkerEnd"

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

function Strip-Block([string]$Target) {
    if (-not (Test-Path $Target)) { return }
    $lines = Get-Content -LiteralPath $Target
    $out = New-Object System.Collections.Generic.List[string]
    $inBlock = $false
    foreach ($line in $lines) {
        if (-not $inBlock -and $line -eq $MarkerBegin) { $inBlock = $true;  continue }
        if ($inBlock     -and $line -eq $MarkerEnd)   { $inBlock = $false; continue }
        if (-not $inBlock) { $out.Add($line) }
    }
    # Trim trailing empty lines that came from sandwiching the block
    while ($out.Count -gt 0 -and [string]::IsNullOrWhiteSpace($out[$out.Count - 1])) {
        $out.RemoveAt($out.Count - 1)
    }
    if ($out.Count -gt 0) {
        Set-Content -LiteralPath $Target -Value $out -Encoding UTF8
    } else {
        # File only ever held our block — leave it empty rather than deleting.
        Set-Content -LiteralPath $Target -Value "" -Encoding UTF8
    }
}

function Write-Block([string]$Target) {
    if ($DryRun) {
        Write-Info "would write snippet to: $Target"
        return
    }
    $dir = Split-Path -Parent $Target
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }

    if (Test-Path $Target) {
        Strip-Block -Target $Target
        $existing = Get-Content -LiteralPath $Target -Raw
        if (-not [string]::IsNullOrEmpty($existing) -and -not $existing.EndsWith("`n")) {
            Add-Content -LiteralPath $Target -Value "" -Encoding UTF8
        }
        if (-not [string]::IsNullOrWhiteSpace($existing)) {
            Add-Content -LiteralPath $Target -Value "" -Encoding UTF8
        }
        Add-Content -LiteralPath $Target -Value $SnippetBlock -Encoding UTF8
    } else {
        Set-Content -LiteralPath $Target -Value $SnippetBlock -Encoding UTF8
    }
    Write-Ok "wrote: $Target"
}

function Remove-Block([string]$Target) {
    if (-not (Test-Path $Target)) {
        Write-Warn "not present, skipping: $Target"
        return
    }
    if ($DryRun) {
        Write-Info "would strip snippet from: $Target"
        return
    }
    Strip-Block -Target $Target
    Write-Ok "stripped: $Target"
}

function Apply([string]$Target) {
    if ($Uninstall) { Remove-Block -Target $Target } else { Write-Block -Target $Target }
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

Write-Info "Cryo Vault — agent-rules installer"
if ($DryRun) { Write-Warn "dry-run: no files will be modified" }

if (-not $SkipClaude)  { Apply -Target $ClaudeRules }  else { Write-Warn "skipping Claude Code ($ClaudeRules)" }
if (-not $SkipAgents)  { Apply -Target $GeminiRules }  else { Write-Warn "skipping Antigravity / AGENTS.md ($GeminiRules)" }
if (-not $SkipCopilot) { Apply -Target $CopilotRules } else { Write-Warn "skipping VSCode Copilot ($CopilotRules)" }

if ($Uninstall) {
    Write-Ok "done. snippet removed from all targets."
} else {
    Write-Ok "done. agents will now auto-archive conversations to Cryo Vault."
    Write-Info "tip: open a new chat in your editor; the rule is loaded at session start."
}
