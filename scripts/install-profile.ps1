<#
.SYNOPSIS
    Idempotently installs the `teams` terminal launcher on any Windows PC.

.DESCRIPTION
    1. Ensures <RepoRoot>\target\release\teams.exe exists (downloads the prebuilt
       release binary from GitHub `BiteFoundry/teams` via `gh` when missing, so
       Rust/Cargo is NOT required on target PCs).
    2. Registers/updates `# >>> teams launcher >>>` (`function teams`) in PowerShell $PROFILE.
    3. If $HOME\Tools\bin exists, also writes `teams.cmd` there so already-open shells
       find `teams` immediately.

.EXAMPLE
    pwsh -File .\scripts\install-profile.ps1
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [string]$ProfilePath = $PROFILE,
    [string]$RepoRoot = (Split-Path -Parent $PSScriptRoot)
)

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($ProfilePath)) {
    throw 'ProfilePath is empty. Pass -ProfilePath explicitly or run inside a PowerShell host where $PROFILE is defined.'
}

$resolvedRoot = if (Test-Path -LiteralPath $RepoRoot) {
    (Resolve-Path -LiteralPath $RepoRoot).Path
} else {
    $RepoRoot
}

$releaseDir = Join-Path $resolvedRoot 'target\release'
$releaseExe = Join-Path $releaseDir 'teams.exe'

if (-not (Test-Path -LiteralPath $releaseExe)) {
    $gh = Get-Command gh -ErrorAction SilentlyContinue
    if ($gh) {
        Write-Output "[teams] Downloading prebuilt teams.exe from BiteFoundry/teams GitHub Release..."
        New-Item -ItemType Directory -Force -Path $releaseDir | Out-Null
        & $gh.Source release download -R 'BiteFoundry/teams' --pattern 'teams.exe' --dir $releaseDir --clobber
    }
}

$escapedRoot = $resolvedRoot.Replace("'", "''")
$startMarker = '# >>> teams launcher >>>'
$endMarker = '# <<< teams launcher <<<'

$block = @"
$startMarker
# Opens the Microsoft Teams terminal client (CLI/TUI/HTTP/MCP) from anywhere.
function teams {
    `$root = '$escapedRoot'
    `$exe = Join-Path `$root 'target\release\teams.exe'
    if (-not (Test-Path -LiteralPath `$exe)) {
        if (Get-Command gh -ErrorAction SilentlyContinue) {
            `$relDir = Split-Path -Parent `$exe
            New-Item -ItemType Directory -Force -Path `$relDir | Out-Null
            gh release download -R 'BiteFoundry/teams' --pattern 'teams.exe' --dir `$relDir --clobber 2>`$null
        }
    }
    if (-not (Test-Path -LiteralPath `$exe)) {
        cargo build --release --manifest-path (Join-Path `$root 'Cargo.toml')
        if (`$LASTEXITCODE -ne 0) { return }
    }
    & `$exe @args
}
$endMarker
"@.TrimEnd("`r", "`n")

$parentDir = Split-Path -Parent $ProfilePath
if ($parentDir -and -not (Test-Path -LiteralPath $parentDir)) {
    if ($PSCmdlet.ShouldProcess($parentDir, 'Create PowerShell profile parent directory')) {
        New-Item -ItemType Directory -Force -Path $parentDir | Out-Null
    }
}

$existing = ''
if (Test-Path -LiteralPath $ProfilePath) {
    $existing = Get-Content -LiteralPath $ProfilePath -Raw -Encoding UTF8
    if ($null -eq $existing) { $existing = '' }
}

$pattern = '(?ms)^# >>> teams launcher >>>\r?\n.*?^# <<< teams launcher <<<\r?\n?'
if ([regex]::IsMatch($existing, $pattern)) {
    $updated = [regex]::Replace($existing, $pattern, { param($m) $block + [Environment]::NewLine }, 1)
    $action = 'Update teams launcher block'
} elseif ([string]::IsNullOrWhiteSpace($existing)) {
    $updated = $block + [Environment]::NewLine
    $action = 'Create profile with teams launcher block'
} else {
    $trimmed = $existing.TrimEnd("`r", "`n")
    $nl = [Environment]::NewLine
    $updated = $trimmed + $nl + $nl + $block + $nl
    $action = 'Append teams launcher block'
}

if ($updated -ne $existing) {
    if ($PSCmdlet.ShouldProcess($ProfilePath, $action)) {
        $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($ProfilePath, $updated, $utf8NoBom)
        Write-Output "$action in $ProfilePath."
    }
} else {
    Write-Output "teams launcher in $ProfilePath is already up to date."
}

$toolsBin = Join-Path $HOME 'Tools\bin'
if (Test-Path -LiteralPath $toolsBin) {
    $teamsShimPath = Join-Path $toolsBin 'teams.cmd'
    $teamsShimContent = "@echo off`r`nchcp 65001 >nul`r`n`"$resolvedRoot\teams.cmd`" %*`r`n"
    if ($PSCmdlet.ShouldProcess($teamsShimPath, 'Write PATH shim teams.cmd')) {
        [System.IO.File]::WriteAllText($teamsShimPath, $teamsShimContent, [System.Text.Encoding]::ASCII)
        Write-Output "Updated PATH shim: $teamsShimPath"
    }
}
