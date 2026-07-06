# build_safe.ps1 -- build Solomon without ever clobbering a running exe.
#
# Constraints this script exists to enforce (see docs/rsi/FAILURE-CATALOG-2026-07-06.md #6):
#   - cargo writes ONLY to src-tauri\target\ during build; the repo root is untouched
#     until an explicit, lock-checked copy step.
#   - repo-root Solomon.exe is replaced ONLY when no solomon process is alive; a live
#     GUI is never overwritten and never renamed aside.
#   - no .bak / rename-aside artifacts, ever. The previous hand-built .bak/.bak2/.bak3
#     chain is exactly the controller-clean violation this replaces.
#
# Usage:
#   powershell -File tools\build_safe.ps1              # build, then copy if safe
#   powershell -File tools\build_safe.ps1 -CopyOnly    # skip build, copy existing artifact if safe
#   powershell -File tools\build_safe.ps1 -CheckOnly   # parse/selfcheck only, no build, exit 0
#
# PowerShell 5.1-safe: no &&, no ternary, explicit exit-code checks (no global
# ErrorActionPreference=Stop so native stderr from cargo can never fake a failure).

[CmdletBinding()]
param(
    [switch]$CopyOnly,
    [switch]$CheckOnly
)

# tools\build_safe.ps1 -> repo root is one level up from this script's directory.
$RepoRoot  = Split-Path -Parent $PSScriptRoot
$SrcTauri  = Join-Path $RepoRoot 'src-tauri'
$TargetExe = Join-Path $SrcTauri 'target\release\solomon.exe'
$RootExe   = Join-Path $RepoRoot 'Solomon.exe'

if ($CheckOnly) {
    # Runnable check contract: reaching this line proves the script parses and its
    # path derivation is sane. No build, no copy, no process queries.
    if (-not (Test-Path $SrcTauri)) {
        Write-Output "build_safe.ps1 selfcheck: WARN src-tauri not found at $SrcTauri"
    }
    Write-Output 'build_safe.ps1 selfcheck: parse OK'
    exit 0
}

if (-not $CopyOnly) {
    if (-not (Test-Path $SrcTauri)) {
        Write-Output "build_safe.ps1: src-tauri not found at $SrcTauri"
        exit 1
    }
    # Build with cwd = src-tauri so all artifacts land in src-tauri\target\release.
    Push-Location $SrcTauri
    cargo build --release
    $buildExit = $LASTEXITCODE
    Pop-Location
    if ($buildExit -ne 0) {
        Write-Output "build_safe.ps1: cargo build --release failed (exit $buildExit)"
        exit 1
    }
}

if (-not (Test-Path $TargetExe)) {
    Write-Output "build_safe.ps1: no artifact at $TargetExe (run without -CopyOnly to build first)"
    exit 1
}

# Copy gate: only replace the root exe when nothing is running it. A live process
# means an operator GUI session -- leave both the root exe and the process alone.
$running = Get-Process solomon -ErrorAction SilentlyContinue
if ($running) {
    Write-Output 'Solomon.exe is running -- fresh build left at src-tauri\target\release\solomon.exe; close the GUI and re-run with -CopyOnly'
    exit 0
}

Copy-Item -Path $TargetExe -Destination $RootExe -Force
Write-Output "build_safe.ps1: copied $TargetExe -> $RootExe"
exit 0
