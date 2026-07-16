<#
.SYNOPSIS
  Idempotent ONE-SHOT launcher for the Asmodeus supervisor, invoked by Solomon's
  gated live_deploy path with cwd = the asmodeus repo root.

.DESCRIPTION
  Lives in Solomon's tree (always checked out on main) so the launch seam works
  regardless of which rsi/iter-* branch the asmodeus checkout is parked on.
  Behavior-identical to asmodeus scripts/asmodeus_keepalive.ps1 (eaa4a4c) except
  the repo root comes from the CURRENT DIRECTORY, not the script location.

    - KILL marker present -> refuse (operator halt always wins; NEVER cleared here)
    - Asmodeus.exe already running -> no-op
    - otherwise -> launch target\release\Asmodeus.exe windowed (visible GUI, taskbar-clickable;
      operator 2026-07-16: apps run as their windowed exe versions — the old headless/hidden
      launch is deleted)

  NOT a background loop: cadence comes only from whoever invokes it (Solomon's
  visible watchdog, or the operator). KILL/DRAIN markers always win.
#>
[CmdletBinding()]
param(
    [switch]$DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = (Get-Location).Path
if ($env:ASMODEUS_HOME) {
    $DataRoot = $env:ASMODEUS_HOME
} else {
    $DataRoot = Join-Path $env:LOCALAPPDATA 'Asmodeus'
}
$KillFile = Join-Path (Join-Path $DataRoot 'state') 'KILL'
$LogFile  = Join-Path $DataRoot 'keepalive.log'
$Exe      = Join-Path $RepoRoot 'target\release\Asmodeus.exe'

function Write-KeepaliveLog {
    param([string]$Message)
    $ts = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    try {
        $dir = Split-Path -Parent $LogFile
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
        Add-Content -Path $LogFile -Value "$ts $Message" -Encoding utf8
    } catch { }
    Write-Output "$ts $Message"
}

if (Test-Path $KillFile) {
    Write-KeepaliveLog "halted: KILL marker present ($KillFile) - not relaunching (solomon seam)"
    exit 0
}

$existing = Get-CimInstance Win32_Process -Filter "Name='Asmodeus.exe'" -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($existing) {
    Write-KeepaliveLog "ok: Asmodeus already running (pid=$($existing.ProcessId)) - no relaunch (solomon seam)"
    exit 0
}

if (-not (Test-Path $Exe)) {
    Write-KeepaliveLog "error: missing binary $Exe - build first (cargo build --release) (solomon seam)"
    exit 1
}

if ($DryRun) {
    Write-KeepaliveLog "dry-run: would launch `"$Exe`" windowed (cwd=$RepoRoot) (solomon seam)"
    exit 0
}

Start-Process -FilePath $Exe -WorkingDirectory $RepoRoot
Start-Sleep -Seconds 3
$post = Get-CimInstance Win32_Process -Filter "Name='Asmodeus.exe'" -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($post) {
    Write-KeepaliveLog "launched: `"$Exe`" windowed (pid=$($post.ProcessId), cwd=$RepoRoot) (solomon seam)"
} else {
    Write-KeepaliveLog "error: launch attempted but no Asmodeus.exe process visible (solomon seam)"
    exit 1
}
